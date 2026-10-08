//! Files API, Vector Stores API and knowledge-search clients of the
//! OpenAI-compatible storage providers (`storage_kind = openai | azure`), and
//! the Anthropic Files client for secondary image copies.

use bytes::Bytes;
use http::{Method, StatusCode};
use oagw_sdk::{Body, MultipartBody, Part};
use serde_json::{Value, json};

use super::client::{CallError, HttpOutcome, LlmClient};
use super::registry::StorageTarget;

/// Error of a storage call.
#[derive(Debug, Clone)]
pub enum StorageError {
    /// Transient failure (gateway error or provider 5xx / 429).
    Transient(String),
    /// Any other failure.
    Permanent(String),
}

impl std::fmt::Display for StorageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Transient(m) => write!(f, "transient storage error: {m}"),
            Self::Permanent(m) => write!(f, "storage error: {m}"),
        }
    }
}

fn from_call(e: &CallError) -> StorageError {
    StorageError::Transient(e.to_string())
}

fn from_status(out: &HttpOutcome, what: &str) -> StorageError {
    let body = String::from_utf8_lossy(&out.body);
    let msg = format!(
        "{what}: HTTP {} {}",
        out.status.as_u16(),
        truncate(&body, 300)
    );
    if out.status.is_server_error()
        || out.status == StatusCode::TOO_MANY_REQUESTS
        || out.from_gateway
    {
        StorageError::Transient(msg)
    } else {
        StorageError::Permanent(msg)
    }
}

fn truncate(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

/// Provider vector-store file indexing status.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IndexStatus {
    InProgress,
    Completed,
    Failed(String),
}

fn index_status(v: &Value) -> IndexStatus {
    match v.get("status").and_then(Value::as_str) {
        None | Some("in_progress") => IndexStatus::InProgress,
        Some("completed") => IndexStatus::Completed,
        Some(other) => IndexStatus::Failed(other.to_owned()),
    }
}

/// Outcome of a delete call: provider 2xx and 404 are success.
fn delete_ok(out: &HttpOutcome, what: &str) -> Result<(), StorageError> {
    if out.status.is_success() || (out.status == StatusCode::NOT_FOUND && !out.from_gateway) {
        Ok(())
    } else {
        Err(from_status(out, what))
    }
}

fn uri(t: &StorageTarget, path: &str) -> String {
    format!("/{}{}{}{}", t.alias, t.prefix(), path, t.query())
}

impl LlmClient {
    /// Upload a file with `purpose = assistants`; returns the provider file id.
    pub async fn upload_file(
        &self,
        t: &StorageTarget,
        filename: &str,
        content_type: &str,
        bytes: Bytes,
    ) -> Result<String, StorageError> {
        let mp = MultipartBody::new().text("purpose", "assistants").part(
            Part::bytes("file", bytes)
                .filename(filename.to_owned())
                .content_type(content_type.to_owned()),
        );
        let ct = mp.content_type();
        let out = self
            .send_buffered(
                None,
                Method::POST,
                &uri(t, "/files"),
                Some(&ct),
                &[],
                mp.into_body(),
            )
            .await
            .map_err(|e| from_call(&e))?;
        if !out.is_success() {
            return Err(from_status(&out, "file upload"));
        }
        out.json()
            .get("id")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| StorageError::Permanent("file upload: response has no id".into()))
    }

    pub async fn delete_file(&self, t: &StorageTarget, file_id: &str) -> Result<(), StorageError> {
        let out = self
            .send_buffered(
                None,
                Method::DELETE,
                &uri(t, &format!("/files/{file_id}")),
                None,
                &[],
                Body::Empty,
            )
            .await
            .map_err(|e| from_call(&e))?;
        delete_ok(&out, "file delete")
    }

    pub async fn create_vector_store(
        &self,
        t: &StorageTarget,
        name: &str,
    ) -> Result<String, StorageError> {
        let body = serde_json::to_vec(&json!({ "name": name })).unwrap_or_default();
        let out = self
            .send_buffered(
                None,
                Method::POST,
                &uri(t, "/vector_stores"),
                Some("application/json"),
                &[],
                Body::from(body),
            )
            .await
            .map_err(|e| from_call(&e))?;
        if !out.is_success() {
            return Err(from_status(&out, "vector store create"));
        }
        out.json()
            .get("id")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| StorageError::Permanent("vector store create: no id".into()))
    }

    pub async fn add_vector_store_file(
        &self,
        t: &StorageTarget,
        vector_store_id: &str,
        file_id: &str,
        attachment_id: &str,
    ) -> Result<IndexStatus, StorageError> {
        let body = serde_json::to_vec(&json!({
            "file_id": file_id,
            "attributes": { "attachment_id": attachment_id },
        }))
        .unwrap_or_default();
        let out = self
            .send_buffered(
                None,
                Method::POST,
                &uri(t, &format!("/vector_stores/{vector_store_id}/files")),
                Some("application/json"),
                &[],
                Body::from(body),
            )
            .await
            .map_err(|e| from_call(&e))?;
        if !out.is_success() {
            return Err(from_status(&out, "vector store file add"));
        }
        Ok(index_status(&out.json()))
    }

    pub async fn get_vector_store_file(
        &self,
        t: &StorageTarget,
        vector_store_id: &str,
        file_id: &str,
    ) -> Result<IndexStatus, StorageError> {
        let out = self
            .send_buffered(
                None,
                Method::GET,
                &uri(
                    t,
                    &format!("/vector_stores/{vector_store_id}/files/{file_id}"),
                ),
                None,
                &[],
                Body::Empty,
            )
            .await
            .map_err(|e| from_call(&e))?;
        if !out.is_success() {
            return Err(from_status(&out, "vector store file status"));
        }
        Ok(index_status(&out.json()))
    }

    pub async fn delete_vector_store(
        &self,
        t: &StorageTarget,
        vector_store_id: &str,
    ) -> Result<(), StorageError> {
        let out = self
            .send_buffered(
                None,
                Method::DELETE,
                &uri(t, &format!("/vector_stores/{vector_store_id}")),
                None,
                &[],
                Body::Empty,
            )
            .await
            .map_err(|e| from_call(&e))?;
        delete_ok(&out, "vector store delete")
    }

    /// Azure knowledge search over an organization vector store.
    pub async fn search_vector_store(
        &self,
        alias: &str,
        api_version: &str,
        vector_store_id: &str,
        query: &str,
        top_k: usize,
    ) -> Result<Vec<(String, String)>, StorageError> {
        let body = serde_json::to_vec(&json!({ "query": query, "max_num_results": top_k }))
            .unwrap_or_default();
        let path = format!(
            "/{alias}/openai/vector_stores/{vector_store_id}/search?api-version={api_version}"
        );
        let out = self
            .send_buffered(
                None,
                Method::POST,
                &path,
                Some("application/json"),
                &[],
                Body::from(body),
            )
            .await
            .map_err(|e| from_call(&e))?;
        if !out.is_success() {
            return Err(from_status(&out, "knowledge search"));
        }
        let v = out.json();
        let mut chunks = Vec::new();
        if let Some(data) = v.get("data").and_then(Value::as_array) {
            for d in data {
                let name = d
                    .get("filename")
                    .and_then(Value::as_str)
                    .unwrap_or("document")
                    .to_owned();
                let text: String = d
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
                chunks.push((name, text));
            }
        }
        Ok(chunks)
    }

    /// Upload a secondary image copy to the Anthropic Files API.
    pub async fn anthropic_upload(
        &self,
        alias: &str,
        filename: &str,
        content_type: &str,
        bytes: Bytes,
    ) -> Result<String, StorageError> {
        let mp = MultipartBody::new().part(
            Part::bytes("file", bytes)
                .filename(filename.to_owned())
                .content_type(content_type.to_owned()),
        );
        let ct = mp.content_type();
        let out = self
            .send_buffered(
                None,
                Method::POST,
                &format!("/{alias}/v1/files"),
                Some(&ct),
                &[
                    (
                        "anthropic-version",
                        super::adapters::anthropic::ANTHROPIC_VERSION,
                    ),
                    ("anthropic-beta", "files-api-2025-04-14"),
                ],
                mp.into_body(),
            )
            .await
            .map_err(|e| from_call(&e))?;
        if !out.is_success() {
            return Err(from_status(&out, "anthropic file upload"));
        }
        out.json()
            .get("id")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| StorageError::Permanent("anthropic upload: no id".into()))
    }

    pub async fn anthropic_delete(&self, alias: &str, file_id: &str) -> Result<(), StorageError> {
        let out = self
            .send_buffered(
                None,
                Method::DELETE,
                &format!("/{alias}/v1/files/{file_id}"),
                None,
                &[
                    (
                        "anthropic-version",
                        super::adapters::anthropic::ANTHROPIC_VERSION,
                    ),
                    ("anthropic-beta", "files-api-2025-04-14"),
                ],
                Body::Empty,
            )
            .await
            .map_err(|e| from_call(&e))?;
        delete_ok(&out, "anthropic file delete")
    }
}
