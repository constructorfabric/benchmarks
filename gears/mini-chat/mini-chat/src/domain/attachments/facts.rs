//! The two fact algorithms of the feature and the reads of the tool contributions
//! (`cpt-cf-mini-chat-algo-attachments-and-file-search-ready-document-fact`,
//! `-ready-code-interpreter-fact`, `-file-search-contribution`): the ready document count and
//! the ready code interpreter files, the chat's bound vector store and the citation file list.
//!
//! This feature owns both predicates; the send preflight (`infra::storage::turn_repo::chat_facts`
//! through `StreamStore::read_preflight`), the quota estimate and the contributions use no other
//! definition. Every read is scoped to `tenant_id` and `chat_id` of the owner-scoped chat.

use async_trait::async_trait;
use toolkit_macros::domain_model;
use uuid::Uuid;

use crate::domain::error::DomainError;

/// A ready attachment with its provider file id.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadyFile {
    pub attachment_id: Uuid,
    pub provider_file_id: String,
    pub filename: String,
}

/// The reads behind the tool contributions.
#[async_trait]
pub trait AttachmentFactsStore: Send + Sync {
    /// The ready document fact: the count of the chat's rows with `status` `ready`,
    /// `deleted_at IS NULL` and `for_file_search` true (never `attachment_kind`).
    ///
    /// # Errors
    ///
    /// `Internal` on a database failure.
    async fn ready_document_count(
        &self,
        tenant_id: Uuid,
        chat_id: Uuid,
    ) -> Result<u32, DomainError>;

    /// The ready code interpreter fact: the chat's rows with `status` `ready`,
    /// `deleted_at IS NULL`, `for_code_interpreter` true and a non-null `provider_file_id`,
    /// ordered by `created_at` (chronologically, also on `SQLite`) then `id`.
    ///
    /// # Errors
    ///
    /// `Internal` on a database failure.
    async fn ready_code_interpreter_files(
        &self,
        tenant_id: Uuid,
        chat_id: Uuid,
    ) -> Result<Vec<ReadyFile>, DomainError>;

    /// The `vector_store_id` of the chat's `chat_vector_stores` row; `None` for no row or a NULL
    /// `vector_store_id` (a placeholder).
    ///
    /// # Errors
    ///
    /// `Internal` on a database failure.
    async fn bound_vector_store_id(
        &self,
        tenant_id: Uuid,
        chat_id: Uuid,
    ) -> Result<Option<String>, DomainError>;

    /// The chat's `ready`, non-deleted attachments with a non-null `provider_file_id` (every
    /// kind), for the citation file map.
    ///
    /// # Errors
    ///
    /// `Internal` on a database failure.
    async fn citation_files(
        &self,
        tenant_id: Uuid,
        chat_id: Uuid,
    ) -> Result<Vec<ReadyFile>, DomainError>;
}
