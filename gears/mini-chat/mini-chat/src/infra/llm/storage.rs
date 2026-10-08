//! Files API and Vector Stores API client (`OpenAI` `/v1/...`, Azure `/openai/...?api-version=`).

use std::sync::Arc;

use bytes::Bytes;
use serde_json::{Value, json};
use toolkit_security::SecurityContext;

use super::provider::ResolvedProvider;
use super::{ProviderRequest, ProviderTransport, TransportError};

/// Storage failure.
#[derive(Debug, Clone, thiserror::Error)]
pub enum StorageError {
    /// Gateway-level failure or provider 5xx (worth retrying).
    #[error("transient storage error: {0}")]
    Transient(String),
    /// Any other non-success.
    #[error("storage error (status {status}): {message}")]
    Failed { status: u16, message: String },
}

/// Outcome of a delete.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeleteOutcome {
    Deleted,
    NotFound,
}

pub struct StorageClient {
    transport: Arc<dyn ProviderTransport>,
}

fn transport_err(e: &TransportError) -> StorageError {
    StorageError::Transient(e.to_string())
}

impl StorageClient {
    #[must_use]
    pub fn new(transport: Arc<dyn ProviderTransport>) -> Self {
        Self { transport }
    }

    async fn call(
        &self,
        ctx: &SecurityContext,
        req: ProviderRequest,
    ) -> Result<(u16, Value), StorageError> {
        let resp = self
            .transport
            .send(ctx.clone(), req)
            .await
            .map_err(|e| transport_err(&e))?;
        let status = resp.status;
        let gateway = resp.gateway;
        let body = resp.into_bytes().await.map_err(StorageError::Transient)?;
        let v: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
        if (200..300).contains(&status) {
            return Ok((status, v));
        }
        let message = v
            .pointer("/error/message")
            .and_then(Value::as_str)
            .or_else(|| v.get("detail").and_then(Value::as_str))
            .unwrap_or("storage request failed")
            .to_owned();
        if gateway || status >= 500 {
            Err(StorageError::Transient(format!(
                "status {status}: {message}"
            )))
        } else {
            Err(StorageError::Failed { status, message })
        }
    }

    /// Uploads a file with `purpose=assistants`. Returns the provider file id.
    ///
    /// # Errors
    /// Storage failure.
    pub async fn upload_file(
        &self,
        ctx: &SecurityContext,
        p: &ResolvedProvider,
        filename: &str,
        content_type: &str,
        data: Bytes,
    ) -> Result<String, StorageError> {
        let boundary = format!("----minichat{}", uuid::Uuid::new_v4().simple());
        let safe_name = filename.replace(['"', '\r', '\n'], "_");
        let mut body = Vec::with_capacity(data.len() + 512);
        body.extend_from_slice(
            format!(
                "--{boundary}\r\nContent-Disposition: form-data; name=\"purpose\"\r\n\r\nassistants\r\n--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{safe_name}\"\r\nContent-Type: {content_type}\r\n\r\n"
            )
            .as_bytes(),
        );
        body.extend_from_slice(&data);
        body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
        let req = ProviderRequest {
            method: http::Method::POST,
            alias: p.alias.clone(),
            path: p.rag_path("/files"),
            content_type: Some(format!("multipart/form-data; boundary={boundary}")),
            accept: None,
            body: Bytes::from(body),
        };
        let (_, v) = self.call(ctx, req).await?;
        v.get("id")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| StorageError::Transient("file upload response has no id".to_owned()))
    }

    /// Deletes a provider file; 404 is reported as `NotFound` (success for cleanup).
    ///
    /// # Errors
    /// Storage failure.
    pub async fn delete_file(
        &self,
        ctx: &SecurityContext,
        p: &ResolvedProvider,
        file_id: &str,
    ) -> Result<DeleteOutcome, StorageError> {
        let req = ProviderRequest::empty(
            http::Method::DELETE,
            &p.alias,
            p.rag_path(&format!("/files/{file_id}")),
        );
        match self.call(ctx, req).await {
            Ok(_) => Ok(DeleteOutcome::Deleted),
            Err(StorageError::Failed { status: 404, .. }) => Ok(DeleteOutcome::NotFound),
            Err(e) => Err(e),
        }
    }

    /// Creates a vector store. Returns its id.
    ///
    /// # Errors
    /// Storage failure.
    pub async fn create_vector_store(
        &self,
        ctx: &SecurityContext,
        p: &ResolvedProvider,
        name: &str,
    ) -> Result<String, StorageError> {
        let req = ProviderRequest::json(
            http::Method::POST,
            &p.alias,
            p.rag_path("/vector_stores"),
            &json!({"name": name}),
        );
        let (_, v) = self.call(ctx, req).await?;
        v.get("id")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| StorageError::Transient("vector store response has no id".to_owned()))
    }

    /// Adds a file to a vector store with the `attachment_id` attribute. Returns the indexing status.
    ///
    /// # Errors
    /// Storage failure.
    pub async fn add_file_to_vector_store(
        &self,
        ctx: &SecurityContext,
        p: &ResolvedProvider,
        vector_store_id: &str,
        file_id: &str,
        attachment_id: uuid::Uuid,
    ) -> Result<Option<String>, StorageError> {
        let req = ProviderRequest::json(
            http::Method::POST,
            &p.alias,
            p.rag_path(&format!("/vector_stores/{vector_store_id}/files")),
            &json!({"file_id": file_id, "attributes": {"attachment_id": attachment_id.to_string()}}),
        );
        let (_, v) = self.call(ctx, req).await?;
        Ok(v.get("status").and_then(Value::as_str).map(str::to_owned))
    }

    /// Reads the indexing status of a vector store file.
    ///
    /// # Errors
    /// Storage failure.
    pub async fn get_vector_store_file_status(
        &self,
        ctx: &SecurityContext,
        p: &ResolvedProvider,
        vector_store_id: &str,
        file_id: &str,
    ) -> Result<Option<String>, StorageError> {
        let req = ProviderRequest::empty(
            http::Method::GET,
            &p.alias,
            p.rag_path(&format!("/vector_stores/{vector_store_id}/files/{file_id}")),
        );
        let (_, v) = self.call(ctx, req).await?;
        Ok(v.get("status").and_then(Value::as_str).map(str::to_owned))
    }

    /// Deletes a vector store; 404 is `NotFound`.
    ///
    /// # Errors
    /// Storage failure.
    pub async fn delete_vector_store(
        &self,
        ctx: &SecurityContext,
        p: &ResolvedProvider,
        vector_store_id: &str,
    ) -> Result<DeleteOutcome, StorageError> {
        let req = ProviderRequest::empty(
            http::Method::DELETE,
            &p.alias,
            p.rag_path(&format!("/vector_stores/{vector_store_id}")),
        );
        match self.call(ctx, req).await {
            Ok(_) => Ok(DeleteOutcome::Deleted),
            Err(StorageError::Failed { status: 404, .. }) => Ok(DeleteOutcome::NotFound),
            Err(e) => Err(e),
        }
    }
}
