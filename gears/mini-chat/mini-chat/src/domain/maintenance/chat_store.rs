//! The persistence port of the chat-cleanup handler (`infra::storage::chat_cleanup_store`).
//! Every method filters on `tenant_id` (and `chat_id` where the row has one): the handler runs
//! outside a user scope, and every write is a guarded conditional update.

use async_trait::async_trait;
use time::OffsetDateTime;
use toolkit_macros::domain_model;
use uuid::Uuid;

use crate::domain::attachments::cleanup::AttemptRecord;
use crate::domain::error::DomainError;

/// The chat row the handler reads (without the deleted filter).
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatFacts {
    /// The model chosen at creation (locked for the life of the chat).
    pub model: String,
    /// `deleted_at` is set.
    pub deleted: bool,
}

/// A `pending` attachment row of the chat.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingAttachment {
    pub id: Uuid,
    /// `None` when the upload never reached the provider.
    pub provider_file_id: Option<String>,
    pub storage_backend: String,
    pub cleanup_attempts: i32,
    pub secondary_file_id: Option<String>,
    /// `none`, `uploaded` or `failed`.
    pub secondary_status: String,
    pub secondary_provider_kind: Option<String>,
}

/// The `chat_vector_stores` row of the chat.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VectorStoreRow {
    pub id: Uuid,
    /// `None` for a creation placeholder.
    pub vector_store_id: Option<String>,
    /// The storage backend label that selects the storage provider.
    pub provider: String,
}

/// The persistence of the chat-cleanup handler.
#[async_trait]
pub trait ChatCleanupStore: Send + Sync {
    /// The chat by `tenant_id` and `chat_id` without the deleted filter.
    ///
    /// # Errors
    ///
    /// `Internal` on a database failure.
    async fn load_chat(
        &self,
        tenant_id: Uuid,
        chat_id: Uuid,
    ) -> Result<Option<ChatFacts>, DomainError>;

    /// The chat's attachment rows (soft-deleted ones included) whose `cleanup_status` is
    /// `pending`, ordered by `created_at` then `id`.
    ///
    /// # Errors
    ///
    /// `Internal` on a database failure.
    async fn pending_attachments(
        &self,
        tenant_id: Uuid,
        chat_id: Uuid,
    ) -> Result<Vec<PendingAttachment>, DomainError>;

    /// The number of the chat's attachment rows still `pending`.
    ///
    /// # Errors
    ///
    /// `Internal` on a database failure.
    async fn count_pending(&self, tenant_id: Uuid, chat_id: Uuid) -> Result<u64, DomainError>;

    /// Whether any attachment row of the chat has `cleanup_status` `failed`.
    ///
    /// # Errors
    ///
    /// `Internal` on a database failure.
    async fn has_failed(&self, tenant_id: Uuid, chat_id: Uuid) -> Result<bool, DomainError>;

    /// Guarded by `cleanup_status` `pending`: `cleanup_status` `done` and `cleanup_updated_at`;
    /// whether a row was updated.
    ///
    /// # Errors
    ///
    /// `Internal` on a database failure.
    async fn mark_done(
        &self,
        tenant_id: Uuid,
        attachment_id: Uuid,
        now: OffsetDateTime,
    ) -> Result<bool, DomainError>;

    /// Guarded by `cleanup_status` `pending`: `cleanup_attempts` plus 1, `last_cleanup_error`
    /// and `cleanup_updated_at`; when `cleanup_attempts` reaches `max_attempts` the same guarded
    /// write sets `cleanup_status` `failed`. `None` when the guard matched no row.
    ///
    /// # Errors
    ///
    /// `Internal` on a database failure.
    async fn record_failed_attempt(
        &self,
        tenant_id: Uuid,
        attachment_id: Uuid,
        cause: &str,
        max_attempts: u32,
        now: OffsetDateTime,
    ) -> Result<Option<AttemptRecord>, DomainError>;

    /// The chat's `chat_vector_stores` row.
    ///
    /// # Errors
    ///
    /// `Internal` on a database failure.
    async fn vector_store(
        &self,
        tenant_id: Uuid,
        chat_id: Uuid,
    ) -> Result<Option<VectorStoreRow>, DomainError>;

    /// Delete the row by `tenant_id`, `chat_id` and `id`; with `placeholder_only` the delete is
    /// guarded by `vector_store_id IS NULL`. Whether a row was deleted.
    ///
    /// # Errors
    ///
    /// `Internal` on a database failure.
    // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-retention:p1:inst-rt-no-purge
    // the chat row and every child row (messages, turns, reactions, attachments with their
    // thumbnails, the thread summary) stay in the database indefinitely: neither handler
    // hard-deletes or soft-deletes them, and the only row the cleanup deletes is this one, the
    // completion marker of the vector store
    async fn delete_vector_store_row(
        &self,
        tenant_id: Uuid,
        chat_id: Uuid,
        id: Uuid,
        placeholder_only: bool,
    ) -> Result<bool, DomainError>;
    // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-retention:p1:inst-rt-no-purge
}
