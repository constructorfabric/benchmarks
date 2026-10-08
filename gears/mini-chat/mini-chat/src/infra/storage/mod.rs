//! File storage and vector stores of the RAG provider (`OpenAI` / Azure `OpenAI`), reached through
//! OAGW with the S2S context. One OpenAI-compatible implementation serves both storage kinds;
//! the paths differ only through [`StorageTarget::uri`](crate::infra::llm::StorageTarget::uri).

use async_trait::async_trait;
use oagw_sdk::body::BodyStream;
use uuid::Uuid;

use crate::infra::llm::StorageTarget;

pub mod anthropic_files;
pub mod azure;
pub mod knowledge;
pub mod openai;

pub use anthropic_files::AnthropicFiles;
pub use openai::OpenAiStorage;

/// Failure of a storage call. Messages never carry provider identifiers.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum StorageError {
    /// Worth retrying: provider 5xx, gateway failures, missing S2S context, invalid responses.
    #[error("storage provider temporarily unavailable: {0}")]
    Transient(String),
    /// The provider rejected the request (4xx other than "not found" on delete).
    #[error("storage provider rejected the request ({status}): {message}")]
    Permanent { status: u16, message: String },
}

/// Indexing state of a file in a vector store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IndexStatus {
    /// Still being indexed (also: the provider sent no `status`).
    InProgress,
    Completed,
    /// `failed`, `cancelled` or an unknown status, with a sanitized reason.
    Failed(String),
}

/// A file to upload; `body` is streamed to the provider without buffering.
pub struct FileUpload {
    pub filename: String,
    pub content_type: String,
    pub body: BodyStream,
}

/// Provider file storage.
#[async_trait]
pub trait FileStorage: Send + Sync {
    /// Uploads the file (`purpose=assistants`) and returns the provider file id.
    async fn upload(&self, t: &StorageTarget, f: FileUpload) -> Result<String, StorageError>;

    /// Deletes the file; a file that does not exist counts as deleted.
    async fn delete(&self, t: &StorageTarget, file_id: &str) -> Result<(), StorageError>;
}

/// Provider vector stores.
#[async_trait]
pub trait VectorStores: Send + Sync {
    /// Creates a vector store called `name` and returns its provider id.
    async fn create(&self, t: &StorageTarget, name: &str) -> Result<String, StorageError>;

    /// Adds the file to the store, tagged with `attachment_id`, and returns its indexing status.
    async fn add_file(
        &self,
        t: &StorageTarget,
        vs_id: &str,
        file_id: &str,
        attachment_id: Uuid,
    ) -> Result<IndexStatus, StorageError>;

    /// Current indexing status of the file in the store.
    async fn file_status(
        &self,
        t: &StorageTarget,
        vs_id: &str,
        file_id: &str,
    ) -> Result<IndexStatus, StorageError>;

    /// Deletes the vector store; a store that does not exist counts as deleted.
    async fn delete(&self, t: &StorageTarget, vs_id: &str) -> Result<(), StorageError>;
}

#[cfg(test)]
mod tests;
