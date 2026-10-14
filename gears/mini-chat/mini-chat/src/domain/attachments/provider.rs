//! The storage-provider port of the upload path: the Files API, the Vector Stores API and the
//! Anthropic Files client, reached through OAGW under the gear's S2S context
//! (`cpt-cf-mini-chat-algo-attachments-and-file-search-provider-file-upload` and the vector
//! store, indexing and secondary-copy algorithms).
//!
//! The OAGW adapter is `infra::oagw::storage_client::OagwStorageClient`.

use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use futures::stream::BoxStream;
use toolkit_macros::domain_model;
use uuid::Uuid;

use crate::config::StorageKind;

/// The storage provider resolved for a tenant: where the Files and Vector Stores calls go.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StorageTarget {
    /// The `providers.<id>` entry that serves storage (the `rag_provider` or the model's own).
    pub provider_id: String,
    /// The OAGW upstream alias for the caller's tenant (`/{alias}{path}`).
    pub upstream_alias: String,
    /// `openai` (`/v1`) or `azure` (`/openai` with `api-version`).
    pub storage_kind: StorageKind,
    /// The Azure `api-version`.
    pub api_version: Option<String>,
    /// The label stored in `attachments.storage_backend` and `chat_vector_stores.provider`.
    pub storage_backend: String,
}

/// Why the byte counter stopped the body of a provider upload.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UploadBodyError {
    /// The byte count exceeded the effective per-file limit.
    LimitExceeded,
    /// The request body could not be read (a client disconnect or a multipart error).
    Read,
}

/// Why a provider file upload did not produce a provider file id.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UploadError {
    /// The body stream failed; the provider request was dropped at once.
    Body(UploadBodyError),
    /// The provider answered non-2xx or the transport failed.
    Provider,
}

/// How a provider call other than the upload failed.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderFailure {
    /// A provider 5xx, a gateway failure or a timeout: worth polling on.
    Transient,
    /// Any other non-success answer (for example a 4xx).
    Other,
    /// A 404 of a file delete (`delete_file`, `delete_secondary_file`, `delete_vector_store`): the
    /// resource is already gone, which the cleanup handler counts as success. No other call
    /// returns it.
    NotFound,
}

/// The provider-side description of an uploaded file.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileUploadMeta {
    /// `{chat_id}_{attachment_id}.{ext}`.
    pub provider_filename: String,
    /// The resolved MIME type.
    pub content_type: String,
}

/// The streamed body of a provider upload; an `Err` item aborts the provider request.
pub type UploadBody = BoxStream<'static, Result<Bytes, UploadBodyError>>;

/// The provider calls of the upload path. Every call is made under the S2S security context and
/// carries no credential; the provider writes are never retried.
#[async_trait]
pub trait StorageProvider: Send + Sync {
    /// `POST /{alias}{prefix}/files`: multipart with `purpose` `assistants` and one `file` part.
    ///
    /// # Errors
    ///
    /// [`UploadError`] when the body stream fails or the provider does not answer 2xx.
    async fn upload_file(
        &self,
        target: &StorageTarget,
        meta: &FileUploadMeta,
        body: UploadBody,
    ) -> Result<String, UploadError>;

    /// `DELETE /{alias}{prefix}/files/{id}`.
    ///
    /// # Errors
    ///
    /// [`ProviderFailure::NotFound`] for a 404, another [`ProviderFailure`] when the provider
    /// does not answer 2xx.
    async fn delete_file(
        &self,
        target: &StorageTarget,
        provider_file_id: &str,
    ) -> Result<(), ProviderFailure>;

    /// `POST /{alias}{prefix}/vector_stores`: the id of the new store.
    ///
    /// # Errors
    ///
    /// [`ProviderFailure`] when the provider does not answer 2xx.
    async fn create_vector_store(&self, target: &StorageTarget) -> Result<String, ProviderFailure>;

    /// `DELETE /{alias}{prefix}/vector_stores/{id}`.
    ///
    /// # Errors
    ///
    /// [`ProviderFailure`] when the provider does not answer 2xx.
    async fn delete_vector_store(
        &self,
        target: &StorageTarget,
        vector_store_id: &str,
    ) -> Result<(), ProviderFailure>;

    /// `POST /{alias}{prefix}/vector_stores/{id}/files` with the provider file id and the single
    /// attribute `attachment_id`: the indexing status text of the response (`None` when the
    /// response has no `status`).
    ///
    /// # Errors
    ///
    /// [`ProviderFailure`] when the provider does not answer 2xx.
    async fn add_vector_store_file(
        &self,
        target: &StorageTarget,
        vector_store_id: &str,
        provider_file_id: &str,
        attachment_id: Uuid,
    ) -> Result<Option<String>, ProviderFailure>;

    /// `GET /{alias}{prefix}/vector_stores/{id}/files/{file_id}` bounded by `timeout`: the
    /// indexing status text (`None` when the response has no `status`).
    ///
    /// # Errors
    ///
    /// [`ProviderFailure::Transient`] for a provider 5xx, a gateway failure or a timeout,
    /// [`ProviderFailure::Other`] for any other failure.
    async fn read_vector_store_file(
        &self,
        target: &StorageTarget,
        vector_store_id: &str,
        provider_file_id: &str,
        timeout: Duration,
    ) -> Result<Option<String>, ProviderFailure>;

    /// The Anthropic Files upload of an image copy: one `file` part, no `purpose`.
    ///
    /// # Errors
    ///
    /// [`ProviderFailure`] when the provider does not answer 2xx.
    async fn upload_secondary_image(
        &self,
        target: &StorageTarget,
        filename: &str,
        content_type: &str,
        bytes: Bytes,
    ) -> Result<String, ProviderFailure>;

    /// `DELETE` of an Anthropic secondary file.
    ///
    /// # Errors
    ///
    /// [`ProviderFailure`] when the provider does not answer 2xx.
    async fn delete_secondary_file(
        &self,
        target: &StorageTarget,
        file_id: &str,
    ) -> Result<(), ProviderFailure>;
}
