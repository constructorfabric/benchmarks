//! File and vector-store operations against the RAG provider (OpenAI or
//! Azure OpenAI, selected by `storage_kind`) through OAGW.

use std::sync::Arc;

use bytes::Bytes;
use http::{Method, StatusCode};
use oagw_sdk::{Body, MultipartBody, Part};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::infra::llm::client::LlmClient;
use crate::infra::llm::resolver::StorageTarget;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum StorageError {
    /// Transient failure (5xx, gateway error): may be retried.
    #[error("transient storage error: {0}")]
    Transient(String),
    /// Non-transient failure.
    #[error("storage error: {0}")]
    Failed(String),
}

/// Vector store file indexing status.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IndexStatus {
    InProgress,
    Completed,
    Failed(String),
}

pub struct StorageClient {
    llm: Arc<LlmClient>,
}

fn status_err(status: StatusCode, body: &[u8]) -> StorageError {
    let msg = format!(
        "HTTP {}: {}",
        status.as_u16(),
        String::from_utf8_lossy(&body[..body.len().min(300)])
    );
    if status.is_server_error() || status == StatusCode::TOO_MANY_REQUESTS {
        StorageError::Transient(msg)
    } else {
        StorageError::Failed(msg)
    }
}

fn parse_index_status(v: &Value) -> IndexStatus {
    match v.get("status").and_then(Value::as_str) {
        None | Some("in_progress") => IndexStatus::InProgress,
        Some("completed") => IndexStatus::Completed,
        Some(other) => IndexStatus::Failed(other.to_owned()),
    }
}

impl StorageClient {
    #[must_use]
    pub fn new(llm: Arc<LlmClient>) -> Self {
        Self { llm }
    }

    async fn call(
        &self,
        method: Method,
        uri: &str,
        headers: &[(&str, String)],
        body: Body,
    ) -> Result<crate::infra::llm::client::RawResponse, StorageError> {
        self.llm
            .raw(method, uri, headers, body)
            .await
            .map_err(|e| StorageError::Transient(format!("gateway: {e}")))
    }

    /// Upload a file (`purpose=assistants`). Returns the provider file id.
    ///
    /// # Errors
    /// Provider or gateway failure.
    pub async fn upload_file(
        &self,
        t: &StorageTarget,
        filename: &str,
        content_type: &str,
        data: Bytes,
        with_purpose: bool,
    ) -> Result<String, StorageError> {
        let mut mp = MultipartBody::new();
        if with_purpose {
            mp = mp.text("purpose", "assistants");
        }
        mp = mp.part(Part::bytes("file", data).filename(filename).content_type(content_type));
        let ct = mp.content_type();
        let resp = self
            .call(Method::POST, &t.uri("/files"), &[("content-type", ct)], mp.into_body())
            .await?;
        if !resp.status.is_success() {
            return Err(status_err(resp.status, &resp.body));
        }
        resp.json()
            .get("id")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| StorageError::Failed("file upload response has no id".to_owned()))
    }

    /// Delete a provider file; 2xx and 404 are success.
    ///
    /// # Errors
    /// Any other status.
    pub async fn delete_file(&self, t: &StorageTarget, file_id: &str) -> Result<(), StorageError> {
        let resp = self
            .call(Method::DELETE, &t.uri(&format!("/files/{file_id}")), &[], Body::Empty)
            .await?;
        if resp.status.is_success() || resp.status == StatusCode::NOT_FOUND {
            Ok(())
        } else {
            Err(status_err(resp.status, &resp.body))
        }
    }

    /// Create a vector store for a chat.
    ///
    /// # Errors
    /// Provider or gateway failure.
    pub async fn create_vector_store(&self, t: &StorageTarget, chat_id: Uuid) -> Result<String, StorageError> {
        let body = serde_json::to_vec(&json!({"name": format!("chat-{chat_id}")})).unwrap_or_default();
        let resp = self
            .call(
                Method::POST,
                &t.uri("/vector_stores"),
                &[("content-type", "application/json".to_owned())],
                Body::from(body),
            )
            .await?;
        if !resp.status.is_success() {
            return Err(status_err(resp.status, &resp.body));
        }
        resp.json()
            .get("id")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| StorageError::Failed("vector store response has no id".to_owned()))
    }

    /// Delete a vector store; 2xx and 404 are success.
    ///
    /// # Errors
    /// Any other status.
    pub async fn delete_vector_store(&self, t: &StorageTarget, vs: &str) -> Result<(), StorageError> {
        let resp = self
            .call(Method::DELETE, &t.uri(&format!("/vector_stores/{vs}")), &[], Body::Empty)
            .await?;
        if resp.status.is_success() || resp.status == StatusCode::NOT_FOUND {
            Ok(())
        } else {
            Err(status_err(resp.status, &resp.body))
        }
    }

    /// Add a file to a vector store with the `attachment_id` attribute.
    ///
    /// # Errors
    /// Provider or gateway failure.
    pub async fn add_file_to_vector_store(
        &self,
        t: &StorageTarget,
        vs: &str,
        file_id: &str,
        attachment_id: Uuid,
    ) -> Result<IndexStatus, StorageError> {
        let body = serde_json::to_vec(&json!({
            "file_id": file_id,
            "attributes": {"attachment_id": attachment_id.to_string()},
        }))
        .unwrap_or_default();
        let resp = self
            .call(
                Method::POST,
                &t.uri(&format!("/vector_stores/{vs}/files")),
                &[("content-type", "application/json".to_owned())],
                Body::from(body),
            )
            .await?;
        if !resp.status.is_success() {
            return Err(status_err(resp.status, &resp.body));
        }
        Ok(parse_index_status(&resp.json()))
    }

    /// Knowledge-base search (Azure OpenAI vector store search). Returns the
    /// text of each result, trimmed to `max_chars`.
    ///
    /// # Errors
    /// Provider or gateway failure.
    pub async fn search_vector_store(
        &self,
        alias: &str,
        api_version: &str,
        vs: &str,
        query: &str,
        top_k: usize,
        max_chars: usize,
    ) -> Result<Vec<String>, StorageError> {
        let uri = format!("/{alias}/openai/vector_stores/{vs}/search?api-version={api_version}");
        let body = serde_json::to_vec(&json!({"query": query, "max_num_results": top_k})).unwrap_or_default();
        let resp = self
            .call(Method::POST, &uri, &[("content-type", "application/json".to_owned())], Body::from(body))
            .await?;
        if !resp.status.is_success() {
            return Err(status_err(resp.status, &resp.body));
        }
        let v = resp.json();
        let mut out = Vec::new();
        if let Some(items) = v.get("data").and_then(Value::as_array) {
            for item in items.iter().take(top_k) {
                let text: String = item
                    .get("content")
                    .and_then(Value::as_array)
                    .map(|parts| {
                        parts
                            .iter()
                            .filter_map(|p| p.get("text").and_then(Value::as_str))
                            .collect::<Vec<_>>()
                            .join("\n")
                    })
                    .unwrap_or_default();
                out.push(text.chars().take(max_chars).collect());
            }
        }
        Ok(out)
    }

    /// Indexing status of a vector store file.
    ///
    /// # Errors
    /// Provider or gateway failure (transient for 5xx).
    pub async fn file_index_status(&self, t: &StorageTarget, vs: &str, file_id: &str) -> Result<IndexStatus, StorageError> {
        let resp = self
            .call(Method::GET, &t.uri(&format!("/vector_stores/{vs}/files/{file_id}")), &[], Body::Empty)
            .await?;
        if !resp.status.is_success() {
            return Err(status_err(resp.status, &resp.body));
        }
        Ok(parse_index_status(&resp.json()))
    }
}
