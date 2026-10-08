//! Files API and Vector Stores API client (`openai` / `azure` storage kinds).

use std::sync::Arc;

use bytes::Bytes;
use oagw_sdk::{Body, MultipartBody, Part};
use serde_json::{Value, json};

use super::resolver::RagTarget;
use super::{ProviderTransport, TransportError};

#[derive(Debug, Clone, thiserror::Error)]
pub enum StorageError {
    /// Gateway failure or provider 5xx/429: may succeed later.
    #[error("transient storage error: {0}")]
    Transient(String),
    /// Any other provider error.
    #[error("storage error: {0}")]
    Permanent(String),
}

impl StorageError {
    #[must_use]
    pub fn is_transient(&self) -> bool {
        matches!(self, Self::Transient(_))
    }
}

/// Indexing status of a vector-store file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IndexStatus {
    InProgress,
    Completed,
    Failed(String),
}

fn parse_status(v: &Value) -> IndexStatus {
    match v.get("status").and_then(Value::as_str) {
        None | Some("in_progress") => IndexStatus::InProgress,
        Some("completed") => IndexStatus::Completed,
        Some(other) => IndexStatus::Failed(other.to_owned()),
    }
}

/// Storage client over the provider transport.
#[derive(Clone)]
pub struct StorageClient {
    transport: Arc<dyn ProviderTransport>,
}

impl StorageClient {
    #[must_use]
    pub fn new(transport: Arc<dyn ProviderTransport>) -> Self {
        Self { transport }
    }

    async fn send(&self, req: http::Request<Body>) -> Result<(http::StatusCode, Value), StorageError> {
        let resp = self.transport.send(req).await.map_err(|e| match e {
            TransportError::Timeout(d) | TransportError::Gateway(d) => StorageError::Transient(d),
        })?;
        let status = resp.status();
        let bytes = resp
            .into_body()
            .into_bytes()
            .await
            .map_err(|e| StorageError::Transient(e.to_string()))?;
        let v: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        Ok((status, v))
    }

    fn classify(status: http::StatusCode, v: &Value) -> StorageError {
        let msg = format!("HTTP {}: {}", status.as_u16(), v);
        if status.is_server_error() || status == http::StatusCode::TOO_MANY_REQUESTS {
            StorageError::Transient(msg)
        } else {
            StorageError::Permanent(msg)
        }
    }

    fn json_request(method: http::Method, uri: String, body: &Value) -> Result<http::Request<Body>, StorageError> {
        http::Request::builder()
            .method(method)
            .uri(uri)
            .header(http::header::CONTENT_TYPE, "application/json")
            .body(Body::from(serde_json::to_vec(body).unwrap_or_default()))
            .map_err(|e| StorageError::Permanent(e.to_string()))
    }

    fn empty_request(method: http::Method, uri: String) -> Result<http::Request<Body>, StorageError> {
        http::Request::builder()
            .method(method)
            .uri(uri)
            .body(Body::Empty)
            .map_err(|e| StorageError::Permanent(e.to_string()))
    }

    /// Uploads a file (`purpose=assistants`) and returns the provider file id.
    ///
    /// # Errors
    /// Transport or provider errors.
    pub async fn upload_file(
        &self,
        rag: &RagTarget,
        filename: &str,
        content_type: &str,
        data: Bytes,
    ) -> Result<String, StorageError> {
        let req = MultipartBody::new()
            .text("purpose", "assistants")
            .part(Part::bytes("file", data).filename(filename.to_owned()).content_type(content_type.to_owned()))
            .into_request(http::Method::POST, rag.uri("/files"))
            .map_err(|e| StorageError::Permanent(e.to_string()))?;
        let (status, v) = self.send(req).await?;
        if !status.is_success() {
            return Err(Self::classify(status, &v));
        }
        v.get("id")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| StorageError::Permanent("file upload response has no id".into()))
    }

    /// Deletes a provider file; 404 counts as success.
    ///
    /// # Errors
    /// Transport or provider errors.
    pub async fn delete_file(&self, rag: &RagTarget, file_id: &str) -> Result<(), StorageError> {
        let (status, v) = self
            .send(Self::empty_request(http::Method::DELETE, rag.uri(&format!("/files/{file_id}")))?)
            .await?;
        if status.is_success() || status == http::StatusCode::NOT_FOUND {
            Ok(())
        } else {
            Err(Self::classify(status, &v))
        }
    }

    /// Creates a vector store and returns its id.
    ///
    /// # Errors
    /// Transport or provider errors.
    pub async fn create_vector_store(&self, rag: &RagTarget, name: &str) -> Result<String, StorageError> {
        let (status, v) = self
            .send(Self::json_request(http::Method::POST, rag.uri("/vector_stores"), &json!({"name": name}))?)
            .await?;
        if !status.is_success() {
            return Err(Self::classify(status, &v));
        }
        v.get("id")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| StorageError::Permanent("vector store response has no id".into()))
    }

    /// Adds a file to a vector store with the `attachment_id` attribute.
    ///
    /// # Errors
    /// Transport or provider errors.
    pub async fn add_file_to_vector_store(
        &self,
        rag: &RagTarget,
        vector_store_id: &str,
        file_id: &str,
        attachment_id: uuid::Uuid,
    ) -> Result<IndexStatus, StorageError> {
        let body = json!({"file_id": file_id, "attributes": {"attachment_id": attachment_id.to_string()}});
        let (status, v) = self
            .send(Self::json_request(
                http::Method::POST,
                rag.uri(&format!("/vector_stores/{vector_store_id}/files")),
                &body,
            )?)
            .await?;
        if !status.is_success() {
            return Err(Self::classify(status, &v));
        }
        Ok(parse_status(&v))
    }

    /// Reads the indexing status of a vector-store file.
    ///
    /// # Errors
    /// Transport or provider errors.
    pub async fn vector_store_file_status(
        &self,
        rag: &RagTarget,
        vector_store_id: &str,
        file_id: &str,
    ) -> Result<IndexStatus, StorageError> {
        let (status, v) = self
            .send(Self::empty_request(
                http::Method::GET,
                rag.uri(&format!("/vector_stores/{vector_store_id}/files/{file_id}")),
            )?)
            .await?;
        if !status.is_success() {
            return Err(Self::classify(status, &v));
        }
        Ok(parse_status(&v))
    }

    /// Deletes a vector store; 404 counts as success.
    ///
    /// # Errors
    /// Transport or provider errors.
    pub async fn delete_vector_store(&self, rag: &RagTarget, vector_store_id: &str) -> Result<(), StorageError> {
        let (status, v) = self
            .send(Self::empty_request(
                http::Method::DELETE,
                rag.uri(&format!("/vector_stores/{vector_store_id}")),
            )?)
            .await?;
        if status.is_success() || status == http::StatusCode::NOT_FOUND {
            Ok(())
        } else {
            Err(Self::classify(status, &v))
        }
    }
}
