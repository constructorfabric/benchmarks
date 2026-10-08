//! Files / vector-store operations through OAGW (`OpenAI` or Azure `OpenAI` storage).

use std::time::Duration;

use bytes::Bytes;
use oagw_sdk::{MultipartBody, Part};
use serde_json::{Value, json};
use uuid::Uuid;

use super::gateway::{BufferedResponse, Gateway, ProxyFailure, empty_request, json_request};
use super::resolver::StorageTarget;

/// Timeout of a storage call.
pub const STORAGE_TIMEOUT: Duration = Duration::from_secs(60);

/// Storage failure.
#[derive(Debug, Clone)]
pub struct StorageError {
    /// Retryable (5xx, gateway failure, timeout).
    pub transient: bool,
    /// Diagnostic (internal only).
    pub detail: String,
}

impl std::fmt::Display for StorageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.detail)
    }
}

/// Indexing status of a vector store file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexStatus {
    /// Still indexing (also when the status is missing).
    InProgress,
    /// Indexed.
    Completed,
    /// `failed`, `cancelled` or unknown.
    Failed,
}

fn failure(f: ProxyFailure) -> StorageError {
    match f {
        ProxyFailure::NotReady => StorageError { transient: true, detail: "gateway not ready".into() },
        ProxyFailure::Timeout(m) | ProxyFailure::Gateway(m) => StorageError { transient: true, detail: m },
    }
}

fn status_error(r: &BufferedResponse, op: &str) -> StorageError {
    StorageError {
        transient: r.status.is_server_error() || r.from_gateway,
        detail: format!("{op}: HTTP {} {}", r.status, String::from_utf8_lossy(&r.body).chars().take(300).collect::<String>()),
    }
}

fn index_status(v: &Value) -> IndexStatus {
    match v.get("status").and_then(Value::as_str) {
        None | Some("in_progress") => IndexStatus::InProgress,
        Some("completed") => IndexStatus::Completed,
        Some(_) => IndexStatus::Failed,
    }
}

/// Storage client.
pub struct StorageClient<'a> {
    gateway: &'a Gateway,
}

impl<'a> StorageClient<'a> {
    /// New client.
    #[must_use]
    pub fn new(gateway: &'a Gateway) -> Self {
        Self { gateway }
    }

    /// Uploads a file (`purpose=assistants`); returns the provider file id.
    ///
    /// # Errors
    /// [`StorageError`].
    pub async fn upload_file(
        &self,
        target: &StorageTarget,
        filename: &str,
        content_type: &str,
        data: Bytes,
    ) -> Result<String, StorageError> {
        let req = MultipartBody::new()
            .text("purpose", "assistants")
            .part(Part::bytes("file", data).filename(filename).content_type(content_type))
            .into_request(http::Method::POST, target.uri("/files"))
            .map_err(|e| StorageError { transient: false, detail: e.to_string() })?;
        let r = self.gateway.send_buffered(req, STORAGE_TIMEOUT).await.map_err(failure)?;
        if !r.status.is_success() {
            return Err(status_error(&r, "file upload"));
        }
        r.json()
            .and_then(|v| v.get("id").and_then(Value::as_str).map(str::to_owned))
            .ok_or_else(|| StorageError { transient: false, detail: "file upload: no id".into() })
    }

    /// Uploads the Anthropic secondary copy (`{alias}/v1/files`, file part only).
    ///
    /// # Errors
    /// [`StorageError`].
    pub async fn upload_anthropic_file(
        &self,
        alias: &str,
        filename: &str,
        content_type: &str,
        data: Bytes,
    ) -> Result<String, StorageError> {
        let req = MultipartBody::new()
            .part(Part::bytes("file", data).filename(filename).content_type(content_type))
            .into_request(http::Method::POST, format!("/{alias}/v1/files"))
            .map_err(|e| StorageError { transient: false, detail: e.to_string() })?;
        let r = self.gateway.send_buffered(req, STORAGE_TIMEOUT).await.map_err(failure)?;
        if !r.status.is_success() {
            return Err(status_error(&r, "anthropic file upload"));
        }
        r.json()
            .and_then(|v| v.get("id").and_then(Value::as_str).map(str::to_owned))
            .ok_or_else(|| StorageError { transient: false, detail: "anthropic upload: no id".into() })
    }

    /// Deletes a provider file at a raw URI (2xx and 404 are success).
    async fn delete_uri(&self, uri: String, op: &str) -> Result<(), StorageError> {
        let r = self
            .gateway
            .send_buffered(empty_request(http::Method::DELETE, &uri), STORAGE_TIMEOUT)
            .await
            .map_err(failure)?;
        if r.status.is_success() || r.status == http::StatusCode::NOT_FOUND {
            Ok(())
        } else {
            Err(status_error(&r, op))
        }
    }

    /// Deletes a provider file (2xx and 404 are success).
    ///
    /// # Errors
    /// [`StorageError`].
    pub async fn delete_file(&self, target: &StorageTarget, file_id: &str) -> Result<(), StorageError> {
        self.delete_uri(target.uri(&format!("/files/{file_id}")), "file delete").await
    }

    /// Deletes an Anthropic secondary file.
    ///
    /// # Errors
    /// [`StorageError`].
    pub async fn delete_anthropic_file(&self, alias: &str, file_id: &str) -> Result<(), StorageError> {
        self.delete_uri(format!("/{alias}/v1/files/{file_id}"), "anthropic file delete").await
    }

    /// Creates a vector store for a chat.
    ///
    /// # Errors
    /// [`StorageError`].
    pub async fn create_vector_store(&self, target: &StorageTarget, chat_id: Uuid) -> Result<String, StorageError> {
        let body = json!({"name": format!("mini-chat-{chat_id}"), "metadata": {"chat_id": chat_id.to_string()}});
        let r = self
            .gateway
            .send_buffered(json_request(http::Method::POST, &target.uri("/vector_stores"), &body), STORAGE_TIMEOUT)
            .await
            .map_err(failure)?;
        if !r.status.is_success() {
            return Err(status_error(&r, "vector store create"));
        }
        r.json()
            .and_then(|v| v.get("id").and_then(Value::as_str).map(str::to_owned))
            .ok_or_else(|| StorageError { transient: false, detail: "vector store create: no id".into() })
    }

    /// Adds a file to a vector store; returns its indexing status.
    ///
    /// # Errors
    /// [`StorageError`].
    pub async fn add_file(
        &self,
        target: &StorageTarget,
        vector_store_id: &str,
        file_id: &str,
        attachment_id: Uuid,
    ) -> Result<IndexStatus, StorageError> {
        let body = json!({"file_id": file_id, "attributes": {"attachment_id": attachment_id.to_string()}});
        let r = self
            .gateway
            .send_buffered(
                json_request(http::Method::POST, &target.uri(&format!("/vector_stores/{vector_store_id}/files")), &body),
                STORAGE_TIMEOUT,
            )
            .await
            .map_err(failure)?;
        if !r.status.is_success() {
            return Err(status_error(&r, "vector store add file"));
        }
        Ok(r.json().map_or(IndexStatus::InProgress, |v| index_status(&v)))
    }

    /// Reads the indexing status of a vector store file.
    ///
    /// # Errors
    /// [`StorageError`] (`transient` for 5xx / gateway failures).
    pub async fn file_status(
        &self,
        target: &StorageTarget,
        vector_store_id: &str,
        file_id: &str,
        timeout: Duration,
    ) -> Result<IndexStatus, StorageError> {
        let uri = target.uri(&format!("/vector_stores/{vector_store_id}/files/{file_id}"));
        let r = self
            .gateway
            .send_buffered(empty_request(http::Method::GET, &uri), timeout)
            .await
            .map_err(failure)?;
        if !r.status.is_success() {
            return Err(status_error(&r, "vector store file status"));
        }
        Ok(r.json().map_or(IndexStatus::InProgress, |v| index_status(&v)))
    }

    /// Deletes a vector store (2xx and 404 are success).
    ///
    /// # Errors
    /// [`StorageError`].
    pub async fn delete_vector_store(&self, target: &StorageTarget, vector_store_id: &str) -> Result<(), StorageError> {
        self.delete_uri(target.uri(&format!("/vector_stores/{vector_store_id}")), "vector store delete").await
    }
}
