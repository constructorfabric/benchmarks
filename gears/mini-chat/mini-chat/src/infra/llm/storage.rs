//! File and vector-store operations dispatched by the storage provider's
//! `storage_kind` (`OpenAI` `/v1` prefix, Azure `/openai` prefix with
//! `api-version`), plus the Anthropic Files client for secondary copies.

use std::sync::Arc;

use oagw_sdk::{Body, MultipartBody, Part, ServiceGatewayClientV1};
use serde_json::Value;
use toolkit_security::SecurityContext;

use super::resolver::ResolvedStorage;
use super::{LlmProviderError, providers::map_gateway_error};
use crate::config::StorageKind;
use crate::infra::sanitize::sanitize_provider_message;

/// The RAG route prefix of a storage kind.
#[must_use]
pub fn prefix(kind: StorageKind) -> &'static str {
    match kind {
        StorageKind::Openai => "/v1",
        StorageKind::Azure => "/openai",
    }
}

/// Append the Azure `api-version` query parameter when required.
fn with_api_version(storage: &ResolvedStorage, path: String) -> String {
    match storage.storage_kind {
        StorageKind::Azure => {
            let version = storage.api_version.clone().unwrap_or_default();
            format!("{path}?api-version={version}")
        }
        StorageKind::Openai => path,
    }
}

/// The vector-store file indexing status.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VsFileStatus {
    /// Indexing in progress (also the fallback for a missing status).
    InProgress,
    /// Indexed.
    Completed,
    /// Failed or cancelled (or an unknown status string).
    Failed,
}

impl VsFileStatus {
    /// Parse a provider status; a missing status counts as in-progress.
    #[must_use]
    pub fn parse(status: Option<&str>) -> Self {
        match status {
            Some("completed") => Self::Completed,
            Some("failed" | "cancelled") => Self::Failed,
            _ => Self::InProgress,
        }
    }
}

/// Storage client over OAGW.
pub struct StorageClient {
    gateway: Arc<dyn ServiceGatewayClientV1>,
}

impl StorageClient {
    /// Create the client.
    #[must_use]
    pub fn new(gateway: Arc<dyn ServiceGatewayClientV1>) -> Self {
        Self { gateway }
    }

    async fn request(
        &self,
        ctx: &SecurityContext,
        method: http::Method,
        alias: &str,
        path: String,
        body: Body,
        content_type: Option<String>,
    ) -> Result<(http::StatusCode, bytes::Bytes), LlmProviderError> {
        let mut builder = http::Request::builder().method(method).uri(format!("/{alias}{path}"));
        if let Some(ct) = content_type {
            builder = builder.header(http::header::CONTENT_TYPE, ct);
        }
        let req = builder
            .body(body)
            .map_err(|e| LlmProviderError::InvalidResponse(format!("request build failed: {e}")))?;
        let resp = self
            .gateway
            .proxy_request(ctx.clone(), req)
            .await
            .map_err(map_gateway_error)?;
        let status = resp.status();
        let bytes = resp
            .into_body()
            .into_bytes()
            .await
            .map_err(|e| LlmProviderError::Unavailable(format!("failed to read response: {e}")))?;
        Ok((status, bytes))
    }

    /// Upload a file (provider Files API, `purpose = assistants`). Returns
    /// the provider file id.
    pub async fn upload_file(
        &self,
        ctx: &SecurityContext,
        storage: &ResolvedStorage,
        filename: &str,
        content_type: &str,
        bytes: bytes::Bytes,
    ) -> Result<String, LlmProviderError> {
        let body = MultipartBody::new()
            .text("purpose", "assistants")
            .part(
                Part::bytes("file", bytes)
                    .filename(filename.to_owned())
                    .content_type(content_type.to_owned()),
            );
        let content_type_header = body.content_type();
        let path = with_api_version(storage, format!("{}/files", prefix(storage.storage_kind)));
        let (status, bytes) = self
            .request(
                ctx,
                http::Method::POST,
                &storage.alias,
                path,
                body.into_body(),
                Some(content_type_header),
            )
            .await?;
        if !status.is_success() {
            return Err(storage_error(status, &bytes));
        }
        let value: Value = serde_json::from_slice(&bytes)
            .map_err(|e| LlmProviderError::InvalidResponse(format!("invalid JSON: {e}")))?;
        value
            .get("id")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| LlmProviderError::InvalidResponse("upload response without id".into()))
    }

    /// Delete a file. 2xx and 404 count as success.
    pub async fn delete_file(
        &self,
        ctx: &SecurityContext,
        storage: &ResolvedStorage,
        file_id: &str,
    ) -> Result<(), LlmProviderError> {
        let path = with_api_version(
            storage,
            format!("{}/files/{file_id}", prefix(storage.storage_kind)),
        );
        let (status, bytes) = self
            .request(ctx, http::Method::DELETE, &storage.alias, path, Body::Empty, None)
            .await?;
        if status.is_success() || status == http::StatusCode::NOT_FOUND {
            Ok(())
        } else {
            Err(storage_error(status, &bytes))
        }
    }

    /// Create a vector store. Returns the provider vector store id.
    pub async fn create_vector_store(
        &self,
        ctx: &SecurityContext,
        storage: &ResolvedStorage,
        name: &str,
    ) -> Result<String, LlmProviderError> {
        let path = with_api_version(
            storage,
            format!("{}/vector_stores", prefix(storage.storage_kind)),
        );
        let body = serde_json::to_vec(&serde_json::json!({"name": name})).unwrap_or_default();
        let (status, bytes) = self
            .request(
                ctx,
                http::Method::POST,
                &storage.alias,
                path,
                Body::from(body),
                Some("application/json".to_owned()),
            )
            .await?;
        if !status.is_success() {
            return Err(storage_error(status, &bytes));
        }
        let value: Value = serde_json::from_slice(&bytes)
            .map_err(|e| LlmProviderError::InvalidResponse(format!("invalid JSON: {e}")))?;
        value
            .get("id")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| LlmProviderError::InvalidResponse("vector store without id".into()))
    }

    /// Add a file to a vector store (with the `attachment_id` attribute).
    /// Returns the indexing status.
    pub async fn add_file_to_vector_store(
        &self,
        ctx: &SecurityContext,
        storage: &ResolvedStorage,
        vector_store_id: &str,
        file_id: &str,
        attachment_id: uuid::Uuid,
    ) -> Result<VsFileStatus, LlmProviderError> {
        let path = with_api_version(
            storage,
            format!(
                "{}/vector_stores/{vector_store_id}/files",
                prefix(storage.storage_kind)
            ),
        );
        let body = serde_json::to_vec(&serde_json::json!({
            "file_id": file_id,
            "attributes": {"attachment_id": attachment_id.to_string()},
        }))
        .unwrap_or_default();
        let (status, bytes) = self
            .request(
                ctx,
                http::Method::POST,
                &storage.alias,
                path,
                Body::from(body),
                Some("application/json".to_owned()),
            )
            .await?;
        if !status.is_success() {
            return Err(storage_error(status, &bytes));
        }
        let value: Value = serde_json::from_slice(&bytes)
            .map_err(|e| LlmProviderError::InvalidResponse(format!("invalid JSON: {e}")))?;
        Ok(VsFileStatus::parse(
            value.get("status").and_then(Value::as_str),
        ))
    }

    /// Read the indexing status of a vector store file.
    pub async fn get_vector_store_file(
        &self,
        ctx: &SecurityContext,
        storage: &ResolvedStorage,
        vector_store_id: &str,
        file_id: &str,
    ) -> Result<VsFileStatus, LlmProviderError> {
        let path = with_api_version(
            storage,
            format!(
                "{}/vector_stores/{vector_store_id}/files/{file_id}",
                prefix(storage.storage_kind)
            ),
        );
        let (status, bytes) = self
            .request(ctx, http::Method::GET, &storage.alias, path, Body::Empty, None)
            .await?;
        if !status.is_success() {
            return Err(storage_error(status, &bytes));
        }
        let value: Value = serde_json::from_slice(&bytes)
            .map_err(|e| LlmProviderError::InvalidResponse(format!("invalid JSON: {e}")))?;
        Ok(VsFileStatus::parse(
            value.get("status").and_then(Value::as_str),
        ))
    }

    /// Delete a vector store. 2xx and 404 count as success.
    pub async fn delete_vector_store(
        &self,
        ctx: &SecurityContext,
        storage: &ResolvedStorage,
        vector_store_id: &str,
    ) -> Result<(), LlmProviderError> {
        let path = with_api_version(
            storage,
            format!(
                "{}/vector_stores/{vector_store_id}",
                prefix(storage.storage_kind)
            ),
        );
        let (status, bytes) = self
            .request(ctx, http::Method::DELETE, &storage.alias, path, Body::Empty, None)
            .await?;
        if status.is_success() || status == http::StatusCode::NOT_FOUND {
            Ok(())
        } else {
            Err(storage_error(status, &bytes))
        }
    }

    /// Upload a secondary copy to the Anthropic Files API (no `purpose`).
    /// Returns the Anthropic file id.
    pub async fn anthropic_upload(
        &self,
        ctx: &SecurityContext,
        alias: &str,
        filename: &str,
        content_type: &str,
        bytes: bytes::Bytes,
    ) -> Result<String, LlmProviderError> {
        let body = MultipartBody::new().part(
            Part::bytes("file", bytes)
                .filename(filename.to_owned())
                .content_type(content_type.to_owned()),
        );
        let content_type_header = body.content_type();
        let builder = http::Request::builder()
            .method(http::Method::POST)
            .uri(format!("/{alias}/v1/files"))
            .header(http::header::CONTENT_TYPE, content_type_header)
            .header("anthropic-version", "2023-06-01");
        let req = builder
            .body(body.into_body())
            .map_err(|e| LlmProviderError::InvalidResponse(format!("request build failed: {e}")))?;
        let resp = self
            .gateway
            .proxy_request(ctx.clone(), req)
            .await
            .map_err(map_gateway_error)?;
        let status = resp.status();
        let bytes = resp
            .into_body()
            .into_bytes()
            .await
            .map_err(|e| LlmProviderError::Unavailable(format!("failed to read response: {e}")))?;
        if !status.is_success() {
            return Err(storage_error(status, &bytes));
        }
        let value: Value = serde_json::from_slice(&bytes)
            .map_err(|e| LlmProviderError::InvalidResponse(format!("invalid JSON: {e}")))?;
        value
            .get("id")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| LlmProviderError::InvalidResponse("upload response without id".into()))
    }

    /// Delete a secondary copy from the Anthropic Files API. 2xx and 404
    /// count as success.
    pub async fn anthropic_delete(
        &self,
        ctx: &SecurityContext,
        alias: &str,
        file_id: &str,
    ) -> Result<(), LlmProviderError> {
        let req = http::Request::builder()
            .method(http::Method::DELETE)
            .uri(format!("/{alias}/v1/files/{file_id}"))
            .header("anthropic-version", "2023-06-01")
            .body(Body::Empty)
            .map_err(|e| LlmProviderError::InvalidResponse(format!("request build failed: {e}")))?;
        let resp = self
            .gateway
            .proxy_request(ctx.clone(), req)
            .await
            .map_err(map_gateway_error)?;
        let status = resp.status();
        if status.is_success() || status == http::StatusCode::NOT_FOUND {
            Ok(())
        } else {
            let bytes = resp.into_body().into_bytes().await.unwrap_or_default();
            Err(storage_error(status, &bytes))
        }
    }
}

/// Map a storage HTTP failure to a provider error (sanitized).
fn storage_error(status: http::StatusCode, body: &[u8]) -> LlmProviderError {
    let message = super::providers::parse_provider_error_message(body)
        .unwrap_or_else(|| format!("storage request failed with HTTP {}", status.as_u16()));
    LlmProviderError::Provider {
        status: Some(status.as_u16()),
        message: sanitize_provider_message(&message),
        retry_after: None,
        usage: None,
        provider_response_id: None,
    }
}
