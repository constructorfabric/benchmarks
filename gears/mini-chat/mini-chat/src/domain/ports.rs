//! Ports (traits) the domain depends on; infrastructure provides the adapters
//! and integration tests inject fakes.

use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use futures::stream::BoxStream;
use mini_chat_sdk::{
    AuditPluginError, MiniChatAuditEvent, PolicySnapshot, PublishError, UsageEvent, UserLimits,
};
use tokio_util::sync::CancellationToken;
use toolkit_db::DbTx;
use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::infra::llm::provider_resolver::{ProviderTarget, StorageTarget};
use crate::infra::llm::types::{LlmCompletion, LlmEvent, LlmRequest, ProviderFailure};
use crate::infra::outbox::payloads::{
    AttachmentCleanupPayload, ChatCleanupPayload, ThreadSummaryPayload,
};

/// Model policy access (snapshots, per-user limits, usage publication).
#[async_trait]
pub trait PolicyPort: Send + Sync {
    /// Snapshot of the user's current policy version.
    async fn current_snapshot(&self, user_id: Uuid) -> Result<Arc<PolicySnapshot>, DomainError>;

    /// Snapshot of a specific policy version (e.g. a turn's `policy_version_applied`).
    async fn snapshot_for_version(
        &self,
        user_id: Uuid,
        version: u64,
    ) -> Result<Arc<PolicySnapshot>, DomainError>;

    /// Per-user limits for a policy version.
    async fn user_limits(&self, user_id: Uuid, version: u64) -> Result<UserLimits, DomainError>;

    /// Publish a settled usage event (plugin resolution failure → `Transient`).
    async fn publish_usage(&self, ev: UsageEvent) -> Result<(), PublishError>;
}

/// Outcome of a successful audit emit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuditResolution {
    /// The event was delivered to the audit plugin.
    Delivered,
    /// No audit plugin is registered for the vendor; the event is dropped.
    NoPlugin,
}

/// Audit event delivery.
#[async_trait]
pub trait AuditPort: Send + Sync {
    /// Deliver an audit event. Plugin resolution failures are
    /// `AuditPluginError::Transient`; "no plugin registered" is
    /// `Ok(AuditResolution::NoPlugin)`.
    async fn emit(&self, ev: MiniChatAuditEvent) -> Result<AuditResolution, AuditPluginError>;
}

/// Outbox wakes collected during one transaction; call [`fire_all`] only
/// after that transaction committed (drop them on rollback).
///
/// [`fire_all`]: PendingWakes::fire_all
#[derive(Debug, Default)]
#[must_use = "pending wakes do nothing until fire_all() is called after commit"]
pub struct PendingWakes(Vec<toolkit_db::outbox::Wake>);

impl PendingWakes {
    /// Empty set.
    pub fn new() -> Self {
        Self::default()
    }

    /// Add the wake of one enqueue.
    pub fn push(&mut self, wake: toolkit_db::outbox::Wake) {
        self.0.push(wake);
    }

    /// Move all wakes of `other` into `self`.
    pub fn extend(&mut self, other: Self) {
        self.0.extend(other.0);
    }

    /// Number of collected wakes.
    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Whether no wake was collected.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Wake the outbox sequencers for every enqueued row (post-commit).
    pub fn fire_all(self) {
        for wake in self.0 {
            wake.fire();
        }
    }
}

/// Durable enqueue of outbox messages inside the caller's transaction.
///
/// Every method writes one message on `tx` and pushes its wake into `wakes`;
/// the caller fires `wakes` after the transaction commits.
#[async_trait]
pub trait OutboxPort: Send + Sync {
    /// Usage event (queue `outbox.queue_name`, partition by tenant).
    async fn enqueue_usage(
        &self,
        tx: &DbTx<'_>,
        ev: &UsageEvent,
        wakes: &mut PendingWakes,
    ) -> Result<(), DomainError>;

    /// Audit event (queue `outbox.audit_queue_name`, partition by tenant).
    async fn enqueue_audit(
        &self,
        tx: &DbTx<'_>,
        ev: &MiniChatAuditEvent,
        wakes: &mut PendingWakes,
    ) -> Result<(), DomainError>;

    /// Attachment cleanup (queue `outbox.cleanup_queue_name`, partition by tenant).
    async fn enqueue_attachment_cleanup(
        &self,
        tx: &DbTx<'_>,
        p: &AttachmentCleanupPayload,
        wakes: &mut PendingWakes,
    ) -> Result<(), DomainError>;

    /// Chat cleanup (queue `outbox.chat_cleanup_queue_name`, partition by chat).
    async fn enqueue_chat_cleanup(
        &self,
        tx: &DbTx<'_>,
        p: &ChatCleanupPayload,
        wakes: &mut PendingWakes,
    ) -> Result<(), DomainError>;

    /// Thread summary (queue `outbox.thread_summary_queue_name`, partition by chat).
    async fn enqueue_thread_summary(
        &self,
        tx: &DbTx<'_>,
        p: &ThreadSummaryPayload,
        wakes: &mut PendingWakes,
    ) -> Result<(), DomainError>;
}

/// LLM provider calls through OAGW (`llm_provider`, D§3.2).
#[async_trait]
pub trait LlmPort: Send + Sync {
    /// Start a streaming request. Events are yielded as the provider sends
    /// them (no buffering); the last event is terminal (`Completed`,
    /// `Incomplete` or `Failed`). Firing `cancel` ends the stream and drops
    /// the provider connection. An HTTP-level failure before the stream
    /// starts is returned as `Err`.
    async fn stream(
        &self,
        target: &ProviderTarget,
        req: LlmRequest,
        cancel: CancellationToken,
    ) -> Result<BoxStream<'static, LlmEvent>, ProviderFailure>;

    /// Non-streaming request (thread summary).
    async fn complete(
        &self,
        target: &ProviderTarget,
        req: LlmRequest,
    ) -> Result<LlmCompletion, ProviderFailure>;
}

/// Indexing status of a file in a provider vector store.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexStatus {
    /// Still indexing (also: the response carried no `status`).
    InProgress,
    Completed,
    /// `failed`, `cancelled` or any unknown status.
    Failed,
}

/// Failure of a provider file / vector-store call.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum StorageError {
    /// 5xx, gateway failure or timeout (a retry may help).
    #[error("transient storage error: {0}")]
    Transient(String),
    /// Any other failure (4xx, malformed response, bad configuration).
    #[error("storage error: {0}")]
    Permanent(String),
}

/// Provider file and vector-store operations through OAGW (S§9.3).
///
/// The provider writes are not retried here and carry no idempotency key.
/// Provider-issued ids are returned as plain strings and never reach clients.
#[async_trait]
pub trait StoragePort: Send + Sync {
    /// Upload a file (`purpose=assistants`); returns the provider file id.
    async fn upload_file(
        &self,
        t: &StorageTarget,
        filename: &str,
        content_type: &str,
        bytes: Bytes,
    ) -> Result<String, StorageError>;

    /// Delete a provider file; 2xx and 404 are success.
    async fn delete_file(&self, t: &StorageTarget, file_id: &str) -> Result<(), StorageError>;

    /// Create the vector store of a chat (`name = chat-{chat_id}`); returns its id.
    async fn create_vector_store(
        &self,
        t: &StorageTarget,
        chat_id: Uuid,
    ) -> Result<String, StorageError>;

    /// Add a file to a vector store (attribute `attachment_id`); returns the
    /// indexing status the provider reports.
    async fn add_file_to_vector_store(
        &self,
        t: &StorageTarget,
        vs: &str,
        file_id: &str,
        attachment_id: Uuid,
    ) -> Result<IndexStatus, StorageError>;

    /// Indexing status of a file in a vector store.
    async fn vector_store_file_status(
        &self,
        t: &StorageTarget,
        vs: &str,
        file_id: &str,
    ) -> Result<IndexStatus, StorageError>;

    /// Delete a vector store; 2xx and 404 are success.
    async fn delete_vector_store(&self, t: &StorageTarget, vs: &str) -> Result<(), StorageError>;
}

/// One knowledge-base excerpt returned by a knowledge search.
#[derive(Debug, Clone, PartialEq)]
pub struct KnowledgeChunk {
    pub filename: String,
    pub score: f64,
    pub text: String,
}

/// Organization knowledge search (D§4 "Knowledge Search"); wired only when
/// `knowledge_search.enabled`.
#[async_trait]
pub trait KnowledgeRetriever: Send + Sync {
    /// The best `top_k` chunks for `query`.
    async fn search(&self, query: &str, top_k: u32) -> Result<Vec<KnowledgeChunk>, DomainError>;
}

/// Secondary file copies in the Anthropic Files API (ADR-0005).
#[async_trait]
pub trait SecondaryFilesPort: Send + Sync {
    /// Upload `bytes` to the Files API of the upstream `alias` (only the
    /// `file` part); returns the file id.
    async fn upload(
        &self,
        alias: &str,
        filename: &str,
        content_type: &str,
        bytes: Bytes,
    ) -> Result<String, StorageError>;

    /// Delete a file; 2xx and 404 are success.
    async fn delete(&self, alias: &str, file_id: &str) -> Result<(), StorageError>;
}

/// Thread-summary trigger inputs of a completed turn (S§10.2): the context
/// plan figures of the turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SummaryTriggerInput {
    /// Context assembly dropped recent messages (urgent trigger).
    pub messages_truncated: bool,
    pub assembled_context_tokens: i64,
    pub effective_budget: i64,
    /// A thread summary was part of the context.
    pub summary_applied: bool,
}

/// What the finalization hands to the summary hook of a completed turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SummaryHookInput {
    pub tenant_id: Uuid,
    pub chat_id: Uuid,
    pub turn_id: Uuid,
    pub request_id: Uuid,
    pub trigger: SummaryTriggerInput,
}

/// Result of the summary trigger of one completed turn
/// (`mini_chat_thread_summary_trigger_total{result}`, recorded after the
/// finalization commit for evaluated turns only).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SummaryTriggerResult {
    /// Summaries disabled, or neither truncation nor the proactive
    /// threshold applies (nothing is recorded).
    NotEvaluated,
    /// A thread-summary message was enqueued.
    Scheduled,
    /// The trigger was evaluated but nothing was enqueued (existing summary
    /// without truncation, no earlier message, frontier already at the
    /// target).
    NotNeeded,
}

impl SummaryTriggerResult {
    /// Metric `result` label (`None` for [`Self::NotEvaluated`]).
    #[must_use]
    pub const fn label(self) -> Option<&'static str> {
        match self {
            Self::NotEvaluated => None,
            Self::Scheduled => Some("scheduled"),
            Self::NotNeeded => Some("not_needed"),
        }
    }
}

/// Thread-summary scheduling, called inside the finalization transaction of
/// a CAS-winning completed turn.
#[async_trait]
pub trait SummaryHook: Send + Sync {
    /// Evaluate the trigger and enqueue summary work on `tx`.
    async fn maybe_enqueue(
        &self,
        tx: &DbTx<'_>,
        input: &SummaryHookInput,
        wakes: &mut PendingWakes,
    ) -> Result<SummaryTriggerResult, DomainError>;
}

/// Summary hook that schedules nothing.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoopSummaryHook;

#[async_trait]
impl SummaryHook for NoopSummaryHook {
    async fn maybe_enqueue(
        &self,
        _tx: &DbTx<'_>,
        _input: &SummaryHookInput,
        _wakes: &mut PendingWakes,
    ) -> Result<SummaryTriggerResult, DomainError> {
        Ok(SummaryTriggerResult::NotEvaluated)
    }
}

/// Result of one outbox delivery as decided by a domain handler; the outbox
/// adapter maps it to the platform's `MessageResult`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HandlerOutcome {
    /// Done; the message is acknowledged.
    Ok,
    /// Not done; the shared outbox redelivers it with backoff.
    Retry,
    /// Never succeeds; the message is dead-lettered with the reason.
    Reject(String),
}

/// Thread-summary outbox work (`ThreadSummaryService` is the real runner).
#[async_trait]
pub trait ThreadSummaryRunner: Send + Sync {
    /// Process one delivery of a summary task; `attempt` is 1 on the first
    /// delivery.
    async fn run(&self, payload: ThreadSummaryPayload, attempt: u32) -> HandlerOutcome;
}

/// Summary runner that acknowledges every task without doing anything.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoopThreadSummaryRunner;

#[async_trait]
impl ThreadSummaryRunner for NoopThreadSummaryRunner {
    async fn run(&self, _payload: ThreadSummaryPayload, _attempt: u32) -> HandlerOutcome {
        HandlerOutcome::Ok
    }
}
