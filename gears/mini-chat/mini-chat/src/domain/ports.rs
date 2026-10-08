//! Ports the domain depends on.

use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use mini_chat_sdk::{MiniChatAuditEvent, PolicySnapshot, PublishError, UsageEvent, UserLimits};
use toolkit_security::{AccessScope, SecurityContext};
use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::infra::llm::types::ResolvedStorage;

/// LLM port (implemented by `infra::llm::providers::OagwLlmClient`) and its
/// request / event types.
pub use crate::infra::llm::types::{
    CompletionResult, ContentPart, InputItem, LlmClient, LlmEvent, LlmRequest, ProviderError,
    RawCitation, RequestMetadata, ResolvedProvider, ToolSpec, feature_label, provider_user,
};

/// Actions on the Chat resource (DESIGN section 3.8, per-operation matrix).
/// Sub-resource operations are actions on the parent chat.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ChatAction {
    Create,
    List,
    Read,
    Update,
    Delete,
    ListMessages,
    SendMessage,
    UploadAttachment,
    ReadAttachment,
    DeleteAttachment,
    ReadTurn,
    RetryTurn,
    EditTurn,
    DeleteTurn,
    SetReaction,
    DeleteReaction,
}

impl ChatAction {
    /// Every action, in the order of the DESIGN matrix.
    pub const ALL: &'static [Self] = &[
        Self::Create,
        Self::List,
        Self::Read,
        Self::Update,
        Self::Delete,
        Self::ListMessages,
        Self::SendMessage,
        Self::UploadAttachment,
        Self::ReadAttachment,
        Self::DeleteAttachment,
        Self::ReadTurn,
        Self::RetryTurn,
        Self::EditTurn,
        Self::DeleteTurn,
        Self::SetReaction,
        Self::DeleteReaction,
    ];

    /// The action name sent to the PDP.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Create => "create",
            Self::List => "list",
            Self::Read => "read",
            Self::Update => "update",
            Self::Delete => "delete",
            Self::ListMessages => "list_messages",
            Self::SendMessage => "send_message",
            Self::UploadAttachment => "upload_attachment",
            Self::ReadAttachment => "read_attachment",
            Self::DeleteAttachment => "delete_attachment",
            Self::ReadTurn => "read_turn",
            Self::RetryTurn => "retry_turn",
            Self::EditTurn => "edit_turn",
            Self::DeleteTurn => "delete_turn",
            Self::SetReaction => "set_reaction",
            Self::DeleteReaction => "delete_reaction",
        }
    }
}

/// Authorization (PEP) port. Fail-closed: a refusal is `AuthzDenied`, a PDP that
/// could not evaluate is `AuthzUnavailable`.
#[async_trait]
pub trait AuthzPort: Send + Sync {
    /// Scope for `action` on a chat (`chat_id` is the resource id when the
    /// action targets one chat). Always narrowed to the caller as owner.
    async fn chat_scope(
        &self,
        ctx: &SecurityContext,
        action: ChatAction,
        chat_id: Option<Uuid>,
    ) -> Result<AccessScope, DomainError>;

    /// Permission-only check on the Model catalog resource (no constraints).
    async fn model_access(&self, ctx: &SecurityContext, action: &str) -> Result<(), DomainError>;

    /// Scope for reading the caller's quota usage. Always narrowed to the caller
    /// as owner.
    async fn quota_scope(&self, ctx: &SecurityContext) -> Result<AccessScope, DomainError>;
}

/// Model policy port (DESIGN section 3.2, "Model policy gateway"). A failure to
/// reach or use the plugin is `DomainError::Internal` (surfaces as 500).
#[async_trait]
pub trait PolicyProvider: Send + Sync {
    /// Snapshot at the user's current policy version.
    async fn current(&self, user_id: Uuid) -> Result<Arc<PolicySnapshot>, DomainError>;

    /// Snapshot at an explicit policy version.
    async fn snapshot(
        &self,
        user_id: Uuid,
        version: u64,
    ) -> Result<Arc<PolicySnapshot>, DomainError>;

    /// Per-user limits at an explicit policy version.
    async fn user_limits(&self, user_id: Uuid, version: u64) -> Result<UserLimits, DomainError>;

    /// Hand a usage settlement to the plugin (usage outbox handler).
    async fn publish_usage(&self, ev: UsageEvent) -> Result<(), PublishError>;
}

/// Outcome of delivering one audit event to the audit plugin.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AuditDelivery {
    /// The plugin accepted the event.
    Delivered,
    /// No plugin is registered; the event is acknowledged and dropped.
    NoPlugin,
    /// Not delivered; the outbox retries with backoff.
    Retry(String),
    /// Permanently refused; the outbox dead-letters the event.
    Reject(String),
}

/// Audit port used by the audit outbox handler.
#[async_trait]
pub trait AuditSink: Send + Sync {
    async fn deliver(&self, ev: MiniChatAuditEvent) -> AuditDelivery;
}

/// Indexing state of a file in a provider vector store (DESIGN section 3.6,
/// "File Upload"). A missing provider status is `InProgress`; any status other
/// than `in_progress` / `completed` is `Failed`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IndexStatus {
    InProgress,
    Completed,
    Failed,
}

/// Failure of a file / vector-store call. Gateway errors and provider 5xx / 429
/// are `Transient` (retry later); a provider 404 is `NotFound` (callers treat it
/// as success on delete); any other failure is `Failed`. `Unavailable` means
/// the request was never sent (the S2S identity is not ready yet): retryable
/// like `Transient`, but not a provider response, so cleanup does not count
/// it as a failed delete attempt.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StorageError {
    NotFound,
    Transient(String),
    Failed(String),
    Unavailable(String),
}

impl std::fmt::Display for StorageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound => f.write_str("not found"),
            Self::Transient(m) => write!(f, "transient storage error: {m}"),
            Self::Failed(m) => write!(f, "storage error: {m}"),
            Self::Unavailable(m) => write!(f, "storage request not sent: {m}"),
        }
    }
}

impl std::error::Error for StorageError {}

/// File and vector-store port (implemented by
/// `infra::llm::storage::OagwRagStorage`). `st` selects the provider, alias and
/// API flavour. Deletes return `Ok(())` on 2xx and `Err(NotFound)` on 404.
#[async_trait]
pub trait RagStorage: Send + Sync {
    /// Upload `bytes` as `filename` (purpose `assistants`); returns the provider file id.
    async fn upload_file(
        &self,
        st: &ResolvedStorage,
        filename: &str,
        content_type: &str,
        bytes: Bytes,
    ) -> Result<String, StorageError>;

    async fn delete_file(&self, st: &ResolvedStorage, file_id: &str) -> Result<(), StorageError>;

    /// Create a vector store; returns its id.
    async fn create_vector_store(
        &self,
        st: &ResolvedStorage,
        name: &str,
    ) -> Result<String, StorageError>;

    /// Attach a file to a vector store with the single attribute
    /// `attachment_id`; returns the initial indexing status.
    async fn add_file_to_vector_store(
        &self,
        st: &ResolvedStorage,
        vs: &str,
        file_id: &str,
        attachment_id: Uuid,
    ) -> Result<IndexStatus, StorageError>;

    async fn vector_store_file_status(
        &self,
        st: &ResolvedStorage,
        vs: &str,
        file_id: &str,
    ) -> Result<IndexStatus, StorageError>;

    async fn delete_vector_store(&self, st: &ResolvedStorage, vs: &str)
    -> Result<(), StorageError>;
}
