//! File and vector-store operations of the RAG provider (`OpenAI` / Azure
//! `OpenAI`), the Anthropic Files API (secondary image copies) and the Azure
//! knowledge retriever, all proxied through OAGW.

use bytes::Bytes;
use oagw_sdk::multipart::{MultipartBody, Part};
use serde_json::{Value, json};

use super::anthropic::{ANTHROPIC_FILES_BETA, ANTHROPIC_VERSION};
use super::gateway::{BufferedResponse, ProxyClient, buffer, parse_error_body};
use super::provider::StorageTarget;
use super::types::ProviderErrorKind;

/// Storage operation failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StorageError {
    /// Retryable (provider 5xx / 429 or a gateway failure).
    pub transient: bool,
    pub status: Option<u16>,
    pub message: String,
}

impl std::fmt::Display for StorageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.status {
            Some(s) => write!(f, "storage error (HTTP {s}): {}", self.message),
            None => write!(f, "storage error: {}", self.message),
        }
    }
}

impl StorageError {
    fn gateway(message: impl Into<String>) -> Self {
        Self {
            transient: true,
            status: None,
            message: message.into(),
        }
    }

    fn from_response(resp: &BufferedResponse) -> Self {
        let (_, message) = parse_error_body(&resp.body);
        Self {
            transient: resp.status >= 500 || resp.status == 429,
            status: Some(resp.status),
            message,
        }
    }
}

/// Indexing status of a file in a vector store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IndexStatus {
    InProgress,
    Completed,
    /// `failed`, `cancelled` or an unknown status.
    Failed(String),
}

impl IndexStatus {
    /// Interpret a vector-store-file `status` (missing counts as in
    /// progress).
    #[must_use]
    pub fn from_json(json: &Value) -> Self {
        match json.get("status").and_then(Value::as_str) {
            None | Some("in_progress" | "queued") => Self::InProgress,
            Some("completed") => Self::Completed,
            Some(other) => Self::Failed(other.to_owned()),
        }
    }
}

/// A knowledge-search result chunk.
#[derive(Debug, Clone, PartialEq)]
pub struct KnowledgeChunk {
    pub text: String,
    pub score: Option<f64>,
    pub filename: Option<String>,
}

#[derive(Clone)]
pub struct StorageClient {
    proxy: ProxyClient,
}

impl StorageClient {
    #[must_use]
    pub fn new(proxy: ProxyClient) -> Self {
        Self { proxy }
    }

    async fn json_call(
        &self,
        method: http::Method,
        uri: &str,
        body: Option<&Value>,
        headers: &[(&str, &str)],
    ) -> Result<BufferedResponse, StorageError> {
        self.proxy
            .send_json(method, uri, body, headers)
            .await
            .map_err(|f| {
                let mut e = StorageError::gateway(f.message);
                if f.kind == ProviderErrorKind::Timeout {
                    e.message = format!("timeout: {}", e.message);
                }
                e
            })
    }

    async fn multipart_call(
        &self,
        uri: &str,
        body: MultipartBody,
        headers: &[(&str, &str)],
    ) -> Result<BufferedResponse, StorageError> {
        let mut request = body
            .into_request(http::Method::POST, uri)
            .map_err(|e| StorageError::gateway(format!("invalid multipart request: {e}")))?;
        for (k, v) in headers {
            if let (Ok(name), Ok(value)) = (
                http::header::HeaderName::from_bytes(k.as_bytes()),
                http::header::HeaderValue::from_str(v),
            ) {
                request.headers_mut().insert(name, value);
            }
        }
        let response = self
            .proxy
            .send(request)
            .await
            .map_err(|f| StorageError::gateway(f.message))?;
        buffer(response)
            .await
            .map_err(|f| StorageError::gateway(f.message))
    }

    fn id_of(resp: &BufferedResponse) -> Result<String, StorageError> {
        resp.json()
            .ok()
            .and_then(|j| j.get("id").and_then(Value::as_str).map(str::to_owned))
            .ok_or_else(|| StorageError {
                transient: false,
                status: Some(resp.status),
                message: "provider response has no id".to_owned(),
            })
    }

    /// Upload a file (`purpose=assistants`). Returns the provider file id.
    ///
    /// # Errors
    /// Provider or gateway failure.
    pub async fn upload_file(
        &self,
        target: &StorageTarget,
        filename: &str,
        content_type: &str,
        bytes: Bytes,
    ) -> Result<String, StorageError> {
        let body = MultipartBody::new().text("purpose", "assistants").part(
            Part::bytes("file", bytes)
                .filename(filename)
                .content_type(content_type),
        );
        let resp = self
            .multipart_call(&target.uri("/files"), body, &[])
            .await?;
        if !resp.is_success() {
            return Err(StorageError::from_response(&resp));
        }
        Self::id_of(&resp)
    }

    /// Delete a provider file. 2xx and 404 are success.
    ///
    /// # Errors
    /// Any other status or a gateway failure.
    pub async fn delete_file(
        &self,
        target: &StorageTarget,
        file_id: &str,
    ) -> Result<(), StorageError> {
        let resp = self
            .json_call(
                http::Method::DELETE,
                &target.uri(&format!("/files/{file_id}")),
                None,
                &[],
            )
            .await?;
        if resp.is_success() || resp.status == 404 {
            Ok(())
        } else {
            Err(StorageError::from_response(&resp))
        }
    }

    /// Create a vector store for a chat. Returns its provider id.
    ///
    /// # Errors
    /// Provider or gateway failure.
    pub async fn create_vector_store(
        &self,
        target: &StorageTarget,
        name: &str,
    ) -> Result<String, StorageError> {
        let resp = self
            .json_call(
                http::Method::POST,
                &target.uri("/vector_stores"),
                Some(&json!({"name": name})),
                &[],
            )
            .await?;
        if !resp.is_success() {
            return Err(StorageError::from_response(&resp));
        }
        Self::id_of(&resp)
    }

    /// Add a file to a vector store (attribute `attachment_id`). Returns
    /// the reported indexing status.
    ///
    /// # Errors
    /// Provider or gateway failure.
    pub async fn add_file_to_vector_store(
        &self,
        target: &StorageTarget,
        vector_store_id: &str,
        file_id: &str,
        attachment_id: &str,
    ) -> Result<IndexStatus, StorageError> {
        let resp = self
            .json_call(
                http::Method::POST,
                &target.uri(&format!("/vector_stores/{vector_store_id}/files")),
                Some(&json!({"file_id": file_id, "attributes": {"attachment_id": attachment_id}})),
                &[],
            )
            .await?;
        if !resp.is_success() {
            return Err(StorageError::from_response(&resp));
        }
        Ok(resp
            .json()
            .map_or(IndexStatus::InProgress, |j| IndexStatus::from_json(&j)))
    }

    /// Read the indexing status of a file in a vector store.
    ///
    /// # Errors
    /// Provider or gateway failure (see [`StorageError::transient`]).
    pub async fn vector_store_file_status(
        &self,
        target: &StorageTarget,
        vector_store_id: &str,
        file_id: &str,
    ) -> Result<IndexStatus, StorageError> {
        let resp = self
            .json_call(
                http::Method::GET,
                &target.uri(&format!("/vector_stores/{vector_store_id}/files/{file_id}")),
                None,
                &[],
            )
            .await?;
        if !resp.is_success() {
            return Err(StorageError::from_response(&resp));
        }
        Ok(resp
            .json()
            .map_or(IndexStatus::InProgress, |j| IndexStatus::from_json(&j)))
    }

    /// Delete a vector store. 2xx and 404 are success.
    ///
    /// # Errors
    /// Any other status or a gateway failure.
    pub async fn delete_vector_store(
        &self,
        target: &StorageTarget,
        vector_store_id: &str,
    ) -> Result<(), StorageError> {
        let resp = self
            .json_call(
                http::Method::DELETE,
                &target.uri(&format!("/vector_stores/{vector_store_id}")),
                None,
                &[],
            )
            .await?;
        if resp.is_success() || resp.status == 404 {
            Ok(())
        } else {
            Err(StorageError::from_response(&resp))
        }
    }

    /// Upload a secondary copy of an image to the Anthropic Files API.
    ///
    /// # Errors
    /// Provider or gateway failure.
    pub async fn upload_anthropic_file(
        &self,
        alias: &str,
        filename: &str,
        content_type: &str,
        bytes: Bytes,
    ) -> Result<String, StorageError> {
        let body = MultipartBody::new().part(
            Part::bytes("file", bytes)
                .filename(filename)
                .content_type(content_type),
        );
        let resp = self
            .multipart_call(
                &format!("/{alias}/v1/files"),
                body,
                &[
                    ("anthropic-version", ANTHROPIC_VERSION),
                    ("anthropic-beta", ANTHROPIC_FILES_BETA),
                ],
            )
            .await?;
        if !resp.is_success() {
            return Err(StorageError::from_response(&resp));
        }
        Self::id_of(&resp)
    }

    /// Delete a secondary Anthropic file. 2xx and 404 are success.
    ///
    /// # Errors
    /// Any other status or a gateway failure.
    pub async fn delete_anthropic_file(
        &self,
        alias: &str,
        file_id: &str,
    ) -> Result<(), StorageError> {
        let resp = self
            .json_call(
                http::Method::DELETE,
                &format!("/{alias}/v1/files/{file_id}"),
                None,
                &[
                    ("anthropic-version", ANTHROPIC_VERSION),
                    ("anthropic-beta", ANTHROPIC_FILES_BETA),
                ],
            )
            .await?;
        if resp.is_success() || resp.status == 404 {
            Ok(())
        } else {
            Err(StorageError::from_response(&resp))
        }
    }

    /// Search an Azure `OpenAI` vector store (knowledge search).
    ///
    /// # Errors
    /// Provider or gateway failure.
    pub async fn search_vector_store(
        &self,
        target: &StorageTarget,
        vector_store_id: &str,
        query: &str,
        top_k: usize,
    ) -> Result<Vec<KnowledgeChunk>, StorageError> {
        let resp = self
            .json_call(
                http::Method::POST,
                &target.uri(&format!("/vector_stores/{vector_store_id}/search")),
                Some(&json!({"query": query, "max_num_results": top_k})),
                &[],
            )
            .await?;
        if !resp.is_success() {
            return Err(StorageError::from_response(&resp));
        }
        let json = resp.json().map_err(|m| StorageError {
            transient: false,
            status: Some(resp.status),
            message: m,
        })?;
        let chunks = json
            .get("data")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .map(|item| {
                        let text = item
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
                        KnowledgeChunk {
                            text,
                            score: item.get("score").and_then(Value::as_f64),
                            filename: item
                                .get("filename")
                                .and_then(Value::as_str)
                                .map(str::to_owned),
                        }
                    })
                    .collect()
            })
            .unwrap_or_default();
        Ok(chunks)
    }
}
