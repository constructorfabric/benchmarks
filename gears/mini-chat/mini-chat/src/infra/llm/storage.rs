//! Files API / Vector Stores API dispatch (`OpenAI` or Azure `OpenAI`, selected
//! by the provider `storage_kind`), the Anthropic Files secondary copy and
//! the knowledge-search retriever.

use bytes::Bytes;
use oagw_sdk::body::Body;
use oagw_sdk::multipart::{MultipartBody, Part};
use serde_json::{Value, json};
use toolkit_security::SecurityContext;

use super::{LlmGateway, ProviderError, ResolvedStorage};

#[derive(Debug, Clone)]
pub enum StorageError {
    /// Provider answered with a non-2xx status.
    Status { status: u16, message: String },
    /// Gateway / transport failure.
    Transport(ProviderError),
    /// Response could not be interpreted.
    Invalid(String),
}

impl StorageError {
    /// Transient read errors (5xx, gateway failures) keep indexing polls going.
    #[must_use]
    pub fn is_transient(&self) -> bool {
        match self {
            Self::Status { status, .. } => *status >= 500,
            Self::Transport(_) => true,
            Self::Invalid(_) => false,
        }
    }
}

impl std::fmt::Display for StorageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Status { status, message } => write!(f, "provider status {status}: {message}"),
            Self::Transport(e) => write!(f, "transport: {}", e.message),
            Self::Invalid(m) => write!(f, "invalid response: {m}"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeleteOutcome {
    Deleted,
    NotFound,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VsFileStatus {
    InProgress,
    Completed,
    Failed(String),
}

fn parse_vs_status(v: &Value) -> VsFileStatus {
    match v.get("status").and_then(Value::as_str) {
        None | Some("in_progress") => VsFileStatus::InProgress,
        Some("completed") => VsFileStatus::Completed,
        Some(other) => VsFileStatus::Failed(other.to_owned()),
    }
}

async fn read_json(resp: http::Response<Body>) -> Result<Value, StorageError> {
    let status = resp.status();
    let bytes = resp
        .into_body()
        .into_bytes()
        .await
        .map_err(|e| StorageError::Invalid(e.to_string()))?;
    if !status.is_success() {
        let msg = String::from_utf8_lossy(&bytes).chars().take(300).collect();
        return Err(StorageError::Status {
            status: status.as_u16(),
            message: msg,
        });
    }
    if bytes.is_empty() {
        return Ok(Value::Null);
    }
    serde_json::from_slice(&bytes).map_err(|e| StorageError::Invalid(e.to_string()))
}

fn json_req(
    method: http::Method,
    url: &str,
    body: Option<&Value>,
) -> Result<http::Request<Body>, StorageError> {
    let mut b = http::Request::builder().method(method).uri(url);
    let body = match body {
        Some(v) => {
            b = b.header(http::header::CONTENT_TYPE, "application/json");
            Body::from(serde_json::to_vec(v).map_err(|e| StorageError::Invalid(e.to_string()))?)
        }
        None => Body::Empty,
    };
    b.body(body)
        .map_err(|e| StorageError::Invalid(e.to_string()))
}

impl LlmGateway {
    async fn send(
        &self,
        ctx: &SecurityContext,
        req: http::Request<Body>,
    ) -> Result<http::Response<Body>, StorageError> {
        self.proxy(ctx, req).await.map_err(StorageError::Transport)
    }

    /// Upload a file (`purpose = assistants`). Returns the provider file id.
    ///
    /// # Errors
    /// Provider / transport failure.
    pub async fn upload_file(
        &self,
        ctx: &SecurityContext,
        st: &ResolvedStorage,
        provider_filename: &str,
        content_type: &str,
        data: Bytes,
    ) -> Result<String, StorageError> {
        let req = MultipartBody::new()
            .text("purpose", "assistants")
            .part(
                Part::bytes("file", data)
                    .filename(provider_filename)
                    .content_type(content_type),
            )
            .into_request(http::Method::POST, st.url("/files"))
            .map_err(|e| StorageError::Invalid(e.to_string()))?;
        let v = read_json(self.send(ctx, req).await?).await?;
        v.get("id")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| StorageError::Invalid("file upload response without id".into()))
    }

    /// Delete a provider file (404 counts as success).
    ///
    /// # Errors
    /// Any other non-2xx status or transport failure.
    pub async fn delete_file(
        &self,
        ctx: &SecurityContext,
        st: &ResolvedStorage,
        file_id: &str,
    ) -> Result<DeleteOutcome, StorageError> {
        let req = json_req(
            http::Method::DELETE,
            &st.url(&format!("/files/{file_id}")),
            None,
        )?;
        delete_outcome(self.send(ctx, req).await?).await
    }

    /// Create a vector store; returns its id.
    ///
    /// # Errors
    /// Provider / transport failure.
    pub async fn create_vector_store(
        &self,
        ctx: &SecurityContext,
        st: &ResolvedStorage,
        name: &str,
    ) -> Result<String, StorageError> {
        let req = json_req(
            http::Method::POST,
            &st.url("/vector_stores"),
            Some(&json!({"name": name})),
        )?;
        let v = read_json(self.send(ctx, req).await?).await?;
        v.get("id")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| StorageError::Invalid("vector store response without id".into()))
    }

    /// Add a file to a vector store; returns its indexing status.
    ///
    /// # Errors
    /// Provider / transport failure.
    pub async fn add_vector_store_file(
        &self,
        ctx: &SecurityContext,
        st: &ResolvedStorage,
        vector_store_id: &str,
        file_id: &str,
        attachment_id: &str,
    ) -> Result<VsFileStatus, StorageError> {
        let req = json_req(
            http::Method::POST,
            &st.url(&format!("/vector_stores/{vector_store_id}/files")),
            Some(&json!({"file_id": file_id, "attributes": {"attachment_id": attachment_id}})),
        )?;
        let v = read_json(self.send(ctx, req).await?).await?;
        Ok(parse_vs_status(&v))
    }

    /// Read a vector store file's indexing status.
    ///
    /// # Errors
    /// Provider / transport failure.
    pub async fn get_vector_store_file(
        &self,
        ctx: &SecurityContext,
        st: &ResolvedStorage,
        vector_store_id: &str,
        file_id: &str,
    ) -> Result<VsFileStatus, StorageError> {
        let req = json_req(
            http::Method::GET,
            &st.url(&format!("/vector_stores/{vector_store_id}/files/{file_id}")),
            None,
        )?;
        let v = read_json(self.send(ctx, req).await?).await?;
        Ok(parse_vs_status(&v))
    }

    /// Delete a vector store (404 counts as success).
    ///
    /// # Errors
    /// Any other non-2xx status or transport failure.
    pub async fn delete_vector_store(
        &self,
        ctx: &SecurityContext,
        st: &ResolvedStorage,
        vector_store_id: &str,
    ) -> Result<DeleteOutcome, StorageError> {
        let req = json_req(
            http::Method::DELETE,
            &st.url(&format!("/vector_stores/{vector_store_id}")),
            None,
        )?;
        delete_outcome(self.send(ctx, req).await?).await
    }

    /// Knowledge search over the organization vector store (Azure `OpenAI`).
    ///
    /// # Errors
    /// Provider / transport failure.
    pub async fn knowledge_search(
        &self,
        ctx: &SecurityContext,
        st: &ResolvedStorage,
        vector_store_id: &str,
        query: &str,
        top_k: usize,
    ) -> Result<Vec<String>, StorageError> {
        let req = json_req(
            http::Method::POST,
            &st.url(&format!("/vector_stores/{vector_store_id}/search")),
            Some(&json!({"query": query, "max_num_results": top_k})),
        )?;
        let v = read_json(self.send(ctx, req).await?).await?;
        let mut chunks = Vec::new();
        if let Some(items) = v.get("data").and_then(Value::as_array) {
            for it in items {
                if let Some(content) = it.get("content").and_then(Value::as_array) {
                    for c in content {
                        if let Some(t) = c.get("text").and_then(Value::as_str) {
                            chunks.push(t.to_owned());
                        }
                    }
                } else if let Some(t) = it.get("text").and_then(Value::as_str) {
                    chunks.push(t.to_owned());
                }
            }
        }
        Ok(chunks)
    }

    /// Secondary copy in the Anthropic Files API.
    ///
    /// # Errors
    /// Provider / transport failure.
    pub async fn upload_anthropic_file(
        &self,
        ctx: &SecurityContext,
        alias: &str,
        filename: &str,
        content_type: &str,
        data: Bytes,
    ) -> Result<String, StorageError> {
        let mut req = MultipartBody::new()
            .part(
                Part::bytes("file", data)
                    .filename(filename)
                    .content_type(content_type),
            )
            .into_request(http::Method::POST, format!("/{alias}/v1/files"))
            .map_err(|e| StorageError::Invalid(e.to_string()))?;
        req.headers_mut().insert(
            "anthropic-beta",
            http::HeaderValue::from_static("files-api-2025-04-14"),
        );
        req.headers_mut().insert(
            "anthropic-version",
            http::HeaderValue::from_static("2023-06-01"),
        );
        let v = read_json(self.send(ctx, req).await?).await?;
        v.get("id")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| StorageError::Invalid("file upload response without id".into()))
    }

    /// Delete the Anthropic secondary copy.
    ///
    /// # Errors
    /// Provider / transport failure.
    pub async fn delete_anthropic_file(
        &self,
        ctx: &SecurityContext,
        alias: &str,
        file_id: &str,
    ) -> Result<DeleteOutcome, StorageError> {
        let mut req = json_req(
            http::Method::DELETE,
            &format!("/{alias}/v1/files/{file_id}"),
            None,
        )?;
        req.headers_mut().insert(
            "anthropic-beta",
            http::HeaderValue::from_static("files-api-2025-04-14"),
        );
        req.headers_mut().insert(
            "anthropic-version",
            http::HeaderValue::from_static("2023-06-01"),
        );
        delete_outcome(self.send(ctx, req).await?).await
    }
}

async fn delete_outcome(resp: http::Response<Body>) -> Result<DeleteOutcome, StorageError> {
    let status = resp.status();
    if status == http::StatusCode::NOT_FOUND {
        return Ok(DeleteOutcome::NotFound);
    }
    if status.is_success() {
        return Ok(DeleteOutcome::Deleted);
    }
    let bytes = resp.into_body().into_bytes().await.unwrap_or_default();
    Err(StorageError::Status {
        status: status.as_u16(),
        message: String::from_utf8_lossy(&bytes).chars().take(300).collect(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vector_store_status_parsing() {
        assert_eq!(parse_vs_status(&json!({})), VsFileStatus::InProgress);
        assert_eq!(
            parse_vs_status(&json!({"status": "in_progress"})),
            VsFileStatus::InProgress
        );
        assert_eq!(
            parse_vs_status(&json!({"status": "completed"})),
            VsFileStatus::Completed
        );
        assert_eq!(
            parse_vs_status(&json!({"status": "cancelled"})),
            VsFileStatus::Failed("cancelled".into())
        );
    }

    #[test]
    fn transient_classification() {
        assert!(
            StorageError::Status {
                status: 503,
                message: String::new()
            }
            .is_transient()
        );
        assert!(
            !StorageError::Status {
                status: 400,
                message: String::new()
            }
            .is_transient()
        );
    }
}
