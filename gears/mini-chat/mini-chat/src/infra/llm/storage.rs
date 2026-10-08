//! Files and Vector Stores API calls through OAGW (`OpenAI` and Azure storage kinds).

use std::time::Duration;

use bytes::Bytes;
use oagw_sdk::{MultipartBody, Part};
use serde_json::{Value, json};

use super::{LlmGateway, ProviderCallError, ResolvedProvider};

const STORAGE_TIMEOUT: Duration = Duration::from_secs(60);

/// Indexing status of a vector store file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IndexingStatus {
    InProgress,
    Completed,
    /// `failed`, `cancelled` or an unknown status.
    Failed(String),
}

fn status_of(v: &Value) -> IndexingStatus {
    match v.get("status").and_then(Value::as_str) {
        None | Some("in_progress") => IndexingStatus::InProgress,
        Some("completed") => IndexingStatus::Completed,
        Some(other) => IndexingStatus::Failed(other.to_owned()),
    }
}

impl LlmGateway {
    /// Uploads a file (`purpose=assistants`); returns the provider file id.
    ///
    /// # Errors
    /// Provider failures.
    pub async fn upload_file(
        &self,
        p: &ResolvedProvider,
        filename: &str,
        content_type: &str,
        data: Bytes,
    ) -> Result<String, ProviderCallError> {
        let uri = p.rag_uri("/files");
        let req = MultipartBody::new()
            .text("purpose", "assistants")
            .part(
                Part::bytes("file", data)
                    .filename(filename)
                    .content_type(content_type),
            )
            .into_request("POST", &uri)
            .map_err(|e| ProviderCallError::Provider {
                status: None,
                message: e.to_string(),
                transient: false,
            })?;
        let resp = tokio::time::timeout(STORAGE_TIMEOUT, self.send(req))
            .await
            .map_err(|_| ProviderCallError::Timeout("file upload timed out".to_owned()))??;
        let bytes =
            resp.into_body()
                .into_bytes()
                .await
                .map_err(|e| ProviderCallError::Provider {
                    status: None,
                    message: e.to_string(),
                    transient: true,
                })?;
        let v: Value = serde_json::from_slice(&bytes).map_err(|e| ProviderCallError::Provider {
            status: None,
            message: format!("invalid file upload response: {e}"),
            transient: false,
        })?;
        v.get("id")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| ProviderCallError::Provider {
                status: None,
                message: "file upload response has no id".to_owned(),
                transient: false,
            })
    }

    /// Deletes a provider file (404 is success).
    ///
    /// # Errors
    /// Provider failures.
    pub async fn delete_file(
        &self,
        p: &ResolvedProvider,
        file_id: &str,
    ) -> Result<(), ProviderCallError> {
        self.delete(&p.rag_uri(&format!("/files/{file_id}")), STORAGE_TIMEOUT)
            .await
    }

    /// Creates a vector store; returns its id.
    ///
    /// # Errors
    /// Provider failures.
    pub async fn create_vector_store(
        &self,
        p: &ResolvedProvider,
        name: &str,
    ) -> Result<String, ProviderCallError> {
        let v = self
            .send_json(
                http::Method::POST,
                &p.rag_uri("/vector_stores"),
                Some(&json!({"name": name})),
                STORAGE_TIMEOUT,
            )
            .await?;
        v.get("id")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| ProviderCallError::Provider {
                status: None,
                message: "vector store response has no id".to_owned(),
                transient: false,
            })
    }

    /// Deletes a vector store (404 is success).
    ///
    /// # Errors
    /// Provider failures.
    pub async fn delete_vector_store(
        &self,
        p: &ResolvedProvider,
        vs_id: &str,
    ) -> Result<(), ProviderCallError> {
        self.delete(
            &p.rag_uri(&format!("/vector_stores/{vs_id}")),
            STORAGE_TIMEOUT,
        )
        .await
    }

    /// Adds a file to a vector store with the `attachment_id` attribute; returns its indexing status.
    ///
    /// # Errors
    /// Provider failures.
    pub async fn add_vector_store_file(
        &self,
        p: &ResolvedProvider,
        vs_id: &str,
        file_id: &str,
        attachment_id: &str,
        timeout: Duration,
    ) -> Result<IndexingStatus, ProviderCallError> {
        let v = self
            .send_json(
                http::Method::POST,
                &p.rag_uri(&format!("/vector_stores/{vs_id}/files")),
                Some(&json!({"file_id": file_id, "attributes": {"attachment_id": attachment_id}})),
                timeout,
            )
            .await?;
        Ok(status_of(&v))
    }

    /// Reads the indexing status of a vector store file.
    ///
    /// # Errors
    /// Provider failures.
    pub async fn vector_store_file_status(
        &self,
        p: &ResolvedProvider,
        vs_id: &str,
        file_id: &str,
        timeout: Duration,
    ) -> Result<IndexingStatus, ProviderCallError> {
        let v = self
            .send_json(
                http::Method::GET,
                &p.rag_uri(&format!("/vector_stores/{vs_id}/files/{file_id}")),
                None,
                timeout,
            )
            .await?;
        Ok(status_of(&v))
    }
}
