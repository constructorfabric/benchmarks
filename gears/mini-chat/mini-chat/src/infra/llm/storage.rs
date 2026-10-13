//! Files API and Vector Stores API client over the provider transport (openai / azure storage kinds).
//!
//! `OpenAI` storage kind: `/{alias}/v1/...`; Azure: `/{alias}/openai/...?api-version=...` (`ResolvedProvider::rag_uri`).

use bytes::Bytes;
use serde_json::{Value, json};
use toolkit_security::SecurityContext;

use crate::infra::llm::resolver::ResolvedProvider;
use crate::infra::llm::transport::{
    HttpResponse, MultipartField, OutgoingBody, ProviderTransport, TransportError,
};

/// Storage failure.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum StorageError {
    /// Transport failure or provider 5xx / 429: worth retrying.
    #[error("transient storage error: {0}")]
    Transient(String),
    /// Provider rejected the request (4xx other than 404/429).
    #[error("storage error {status}: {message}")]
    Rejected { status: u16, message: String },
}

/// Indexing status of a vector store file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IndexStatus {
    /// `in_progress` or no status reported.
    InProgress,
    Completed,
    /// `failed`, `cancelled` or an unknown value.
    Failed(String),
}

/// Outcome of a DELETE (404 counts as already deleted).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeleteOutcome {
    Deleted,
    NotFound,
}

/// Uploads a file with `purpose=assistants`; returns the provider file id.
///
/// # Errors
/// `StorageError` on transport failure or non-2xx.
pub async fn upload_file(
    t: &dyn ProviderTransport,
    ctx: &SecurityContext,
    p: &ResolvedProvider,
    filename: &str,
    content_type: &str,
    data: Bytes,
) -> Result<String, StorageError> {
    let fields = vec![
        MultipartField {
            name: "purpose".to_owned(),
            filename: None,
            content_type: None,
            data: Bytes::from_static(b"assistants"),
        },
        MultipartField {
            name: "file".to_owned(),
            filename: Some(filename.to_owned()),
            content_type: Some(content_type.to_owned()),
            data,
        },
    ];
    let resp = send(
        t,
        ctx,
        http::Method::POST,
        &p.rag_uri("/files"),
        OutgoingBody::Multipart(fields),
    )
    .await?;
    json_id(&resp)
}

/// Deletes a provider file (2xx → `Deleted`, 404 → `NotFound`).
///
/// # Errors
/// `StorageError` on any other status or transport failure.
pub async fn delete_file(
    t: &dyn ProviderTransport,
    ctx: &SecurityContext,
    p: &ResolvedProvider,
    file_id: &str,
) -> Result<DeleteOutcome, StorageError> {
    let uri = p.rag_uri(&format!("/files/{file_id}"));
    delete(t, ctx, &uri).await
}

/// Creates a vector store; returns its id.
///
/// # Errors
/// `StorageError` on failure.
pub async fn create_vector_store(
    t: &dyn ProviderTransport,
    ctx: &SecurityContext,
    p: &ResolvedProvider,
    name: &str,
) -> Result<String, StorageError> {
    let resp = send(
        t,
        ctx,
        http::Method::POST,
        &p.rag_uri("/vector_stores"),
        OutgoingBody::Json(json!({"name": name})),
    )
    .await?;
    json_id(&resp)
}

/// Deletes a vector store (404 → `NotFound`).
///
/// # Errors
/// `StorageError` on failure.
pub async fn delete_vector_store(
    t: &dyn ProviderTransport,
    ctx: &SecurityContext,
    p: &ResolvedProvider,
    vector_store_id: &str,
) -> Result<DeleteOutcome, StorageError> {
    let uri = p.rag_uri(&format!("/vector_stores/{vector_store_id}"));
    delete(t, ctx, &uri).await
}

/// Adds a file to a vector store with attribute `attachment_id`; returns the reported status.
///
/// # Errors
/// `StorageError` on failure.
pub async fn add_file_to_vector_store(
    t: &dyn ProviderTransport,
    ctx: &SecurityContext,
    p: &ResolvedProvider,
    vector_store_id: &str,
    file_id: &str,
    attachment_id: uuid::Uuid,
) -> Result<IndexStatus, StorageError> {
    let body =
        json!({"file_id": file_id, "attributes": {"attachment_id": attachment_id.to_string()}});
    let uri = p.rag_uri(&format!("/vector_stores/{vector_store_id}/files"));
    let resp = send(t, ctx, http::Method::POST, &uri, OutgoingBody::Json(body)).await?;
    Ok(index_status(&resp))
}

/// Reads the indexing status of a vector store file.
///
/// # Errors
/// `StorageError` on failure.
pub async fn get_vector_store_file_status(
    t: &dyn ProviderTransport,
    ctx: &SecurityContext,
    p: &ResolvedProvider,
    vector_store_id: &str,
    file_id: &str,
) -> Result<IndexStatus, StorageError> {
    let uri = p.rag_uri(&format!("/vector_stores/{vector_store_id}/files/{file_id}"));
    let resp = send(t, ctx, http::Method::GET, &uri, OutgoingBody::Empty).await?;
    Ok(index_status(&resp))
}

fn transport_error(e: &TransportError) -> StorageError {
    StorageError::Transient(e.to_string())
}

/// Error message of a provider error body (`error.message` / `message`), else the status.
fn error_message(resp: &HttpResponse) -> String {
    let json = resp.json();
    json.pointer("/error/message")
        .and_then(Value::as_str)
        .or_else(|| json.get("error").and_then(Value::as_str))
        .or_else(|| json.get("message").and_then(Value::as_str))
        .map_or_else(
            || format!("provider returned HTTP {}", resp.status),
            str::to_owned,
        )
}

/// Non-2xx → error: 5xx / 429 → `Transient`, other statuses → `Rejected`.
fn status_error(resp: &HttpResponse) -> StorageError {
    let message = error_message(resp);
    if resp.status >= 500 || resp.status == 429 {
        StorageError::Transient(format!("HTTP {}: {message}", resp.status))
    } else {
        StorageError::Rejected {
            status: resp.status,
            message,
        }
    }
}

/// Sends a request and requires a 2xx answer.
async fn send(
    t: &dyn ProviderTransport,
    ctx: &SecurityContext,
    method: http::Method,
    uri: &str,
    body: OutgoingBody,
) -> Result<HttpResponse, StorageError> {
    let resp = t
        .request(ctx, method, uri, body)
        .await
        .map_err(|e| transport_error(&e))?;
    if resp.is_success() {
        Ok(resp)
    } else {
        Err(status_error(&resp))
    }
}

async fn delete(
    t: &dyn ProviderTransport,
    ctx: &SecurityContext,
    uri: &str,
) -> Result<DeleteOutcome, StorageError> {
    let resp = t
        .request(ctx, http::Method::DELETE, uri, OutgoingBody::Empty)
        .await
        .map_err(|e| transport_error(&e))?;
    match resp.status {
        200..=299 => Ok(DeleteOutcome::Deleted),
        404 => Ok(DeleteOutcome::NotFound),
        _ => Err(status_error(&resp)),
    }
}

fn json_id(resp: &HttpResponse) -> Result<String, StorageError> {
    resp.json()
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| StorageError::Transient("provider response has no id".to_owned()))
}

fn index_status(resp: &HttpResponse) -> IndexStatus {
    match resp.json().get("status").and_then(Value::as_str) {
        None | Some("in_progress") => IndexStatus::InProgress,
        Some("completed") => IndexStatus::Completed,
        Some(other) => IndexStatus::Failed(other.to_owned()),
    }
}

#[cfg(test)]
#[path = "storage_tests.rs"]
mod tests;
