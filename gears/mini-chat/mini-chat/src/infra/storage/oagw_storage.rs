//! The [`StorageClient`] over the OAGW in-process proxy
//! (`cpt-cf-mini-chat-algo-attachments-build-storage-request`,
//! `-upload-provider-file`, `-upload-secondary-copy`): the exact requests of
//! the "Storage wire contract" of the FEATURE.

use std::sync::Arc;

use async_trait::async_trait;
use futures::StreamExt;
use oagw_sdk::{Body, MultipartBody, Part, ServiceGatewayClientV1};
use serde_json::{Value, json};
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::config::StorageKind;
use crate::domain::service::attachments::model::StorageTarget;
use crate::domain::service::attachments::ports::{
    FileUpload, IndexStatus, StorageClient, StorageError,
};

/// Largest provider response body that is read.
pub const RESPONSE_BODY_LIMIT: usize = 1024 * 1024;

const ANTHROPIC_VERSION: &str = "2023-06-01";
const ANTHROPIC_BETA: &str = "files-api-2025-04-14";

// @cpt-dod:cpt-cf-mini-chat-dod-attachments-provider-storage:p1
/// OAGW-backed storage client.
pub struct OagwStorageClient {
    oagw: Arc<dyn ServiceGatewayClientV1>,
}

impl OagwStorageClient {
    /// The client over `oagw`.
    #[must_use]
    pub fn new(oagw: Arc<dyn ServiceGatewayClientV1>) -> Self {
        Self { oagw }
    }

    /// Sends `request`; the answer is `(status, body up to 1 MiB)`.
    async fn send(
        &self,
        ctx: &SecurityContext,
        request: http::Request<Body>,
    ) -> Result<(u16, Vec<u8>), StorageError> {
        let response = self
            .oagw
            .proxy_request(ctx.clone(), request)
            .await
            .map_err(|error| {
                tracing::warn!(%error, "the storage request failed before a response");
                StorageError::Transport
            })?;
        let (parts, body) = response.into_parts();
        let mut stream = body.into_stream();
        let mut bytes = Vec::new();
        while bytes.len() < RESPONSE_BODY_LIMIT {
            match stream.next().await {
                Some(Ok(chunk)) => {
                    let room = RESPONSE_BODY_LIMIT - bytes.len();
                    bytes.extend_from_slice(&chunk[..chunk.len().min(room)]);
                }
                Some(Err(_)) | None => break,
            }
        }
        Ok((parts.status.as_u16(), bytes))
    }

    async fn json_call(
        &self,
        ctx: &SecurityContext,
        method: &str,
        uri: String,
        body: Option<Value>,
    ) -> Result<Value, StorageError> {
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-build-storage-request:p1:inst-storage-headers
        let builder = http::Request::builder()
            .method(method)
            .uri(uri)
            .header("accept", "application/json");
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-build-storage-request:p1:inst-storage-headers
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-build-storage-request:p1:inst-return-storage-request
        let request = match body {
            Some(body) => builder
                .header("content-type", "application/json")
                .body(Body::from(body.to_string())),
            None => builder.body(Body::Empty),
        }
        .map_err(|_| StorageError::Transport)?;
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-build-storage-request:p1:inst-return-storage-request
        let (status, bytes) = self.send(ctx, request).await?;
        if !(200..300).contains(&status) {
            return Err(StorageError::Status(status));
        }
        serde_json::from_slice(&bytes).map_err(|_| StorageError::Malformed)
    }

    async fn delete_call(
        &self,
        ctx: &SecurityContext,
        uri: String,
        anthropic: bool,
    ) -> Result<(), StorageError> {
        let mut builder = http::Request::builder().method("DELETE").uri(uri);
        if anthropic {
            builder = builder
                .header("anthropic-version", ANTHROPIC_VERSION)
                .header("anthropic-beta", ANTHROPIC_BETA);
        }
        let request = builder
            .body(Body::Empty)
            .map_err(|_| StorageError::Transport)?;
        let (status, _) = self.send(ctx, request).await?;
        if (200..300).contains(&status) || status == 404 {
            Ok(())
        } else {
            Err(StorageError::Status(status))
        }
    }

    async fn upload_multipart(
        &self,
        ctx: &SecurityContext,
        uri: String,
        multipart: MultipartBody,
        anthropic: bool,
    ) -> Result<String, StorageError> {
        let mut request = multipart
            .into_request("POST", uri)
            .map_err(|_| StorageError::Transport)?;
        request
            .headers_mut()
            .insert("accept", http::HeaderValue::from_static("application/json"));
        if anthropic {
            let headers = request.headers_mut();
            headers.insert(
                "anthropic-version",
                http::HeaderValue::from_static(ANTHROPIC_VERSION),
            );
            headers.insert(
                "anthropic-beta",
                http::HeaderValue::from_static(ANTHROPIC_BETA),
            );
        }
        let (status, bytes) = self.send(ctx, request).await?;
        if !(200..300).contains(&status) {
            return Err(StorageError::Status(status));
        }
        id_of(&serde_json::from_slice(&bytes).map_err(|_| StorageError::Malformed)?)
    }
}

/// Percent-encodes `id` as one path segment.
fn segment(id: &str) -> String {
    let mut out = String::with_capacity(id.len());
    for byte in id.bytes() {
        if byte.is_ascii_alphanumeric() || b"-_.~".contains(&byte) {
            out.push(char::from(byte));
        } else {
            out.push('%');
            for nibble in [byte >> 4, byte & 0x0F] {
                out.push(
                    char::from_digit(u32::from(nibble), 16)
                        .map_or('0', |digit| digit.to_ascii_uppercase()),
                );
            }
        }
    }
    out
}

/// `/{alias}{prefix}{path}` plus the Azure `api-version` query.
fn uri(target: &StorageTarget, path: &str) -> String {
    // @cpt-begin:cpt-cf-mini-chat-algo-attachments-build-storage-request:p1:inst-storage-prefix
    match target.kind {
        StorageKind::Openai => format!("/{}/v1{path}", target.alias),
        StorageKind::Azure => {
            let version = target.api_version.as_deref().unwrap_or_default();
            format!("/{}/openai{path}?api-version={version}", target.alias)
        } // @cpt-end:cpt-cf-mini-chat-algo-attachments-build-storage-request:p1:inst-storage-prefix
    }
}

fn id_of(value: &Value) -> Result<String, StorageError> {
    value
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .map(str::to_owned)
        .ok_or(StorageError::Malformed)
}

fn status_of(value: &Value) -> IndexStatus {
    match value.get("status").and_then(Value::as_str) {
        None | Some("in_progress") => IndexStatus::InProgress,
        Some("completed") => IndexStatus::Completed,
        Some(_) => IndexStatus::Failed,
    }
}

fn file_part(file: &FileUpload) -> Part {
    Part::bytes("file", file.bytes.clone())
        .filename(format!(
            "{}_{}.{}",
            file.chat_id, file.attachment_id, file.extension
        ))
        .content_type(file.content_type.clone())
}

// @cpt-begin:cpt-cf-mini-chat-algo-attachments-build-storage-request:p1:inst-match-operation
#[async_trait]
impl StorageClient for OagwStorageClient {
    async fn upload_file(
        &self,
        ctx: &SecurityContext,
        target: &StorageTarget,
        file: &FileUpload,
    ) -> Result<String, StorageError> {
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-build-storage-request:p1:inst-case-file-upload
        let multipart = MultipartBody::new()
            .part(Part::text("purpose", "assistants"))
            .part(file_part(file));
        self.upload_multipart(ctx, uri(target, "/files"), multipart, false)
            .await
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-build-storage-request:p1:inst-case-file-upload
    }

    async fn upload_secondary(
        &self,
        ctx: &SecurityContext,
        alias: &str,
        file: &FileUpload,
    ) -> Result<String, StorageError> {
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-build-storage-request:p1:inst-case-secondary
        let multipart = MultipartBody::new().part(file_part(file));
        self.upload_multipart(ctx, format!("/{alias}/v1/files"), multipart, true)
            .await
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-build-storage-request:p1:inst-case-secondary
    }

    async fn create_vector_store(
        &self,
        ctx: &SecurityContext,
        target: &StorageTarget,
        chat_id: Uuid,
    ) -> Result<String, StorageError> {
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-build-storage-request:p1:inst-case-store-create
        let value = self
            .json_call(
                ctx,
                "POST",
                uri(target, "/vector_stores"),
                Some(json!({ "name": format!("mini-chat-{chat_id}") })),
            )
            .await?;
        id_of(&value)
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-build-storage-request:p1:inst-case-store-create
    }

    async fn add_file(
        &self,
        ctx: &SecurityContext,
        target: &StorageTarget,
        vector_store_id: &str,
        provider_file_id: &str,
        attachment_id: Uuid,
    ) -> Result<IndexStatus, StorageError> {
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-build-storage-request:p1:inst-case-add-file
        let value = self
            .json_call(
                ctx,
                "POST",
                uri(
                    target,
                    &format!("/vector_stores/{}/files", segment(vector_store_id)),
                ),
                Some(json!({
                    "file_id": provider_file_id,
                    "attributes": { "attachment_id": attachment_id.to_string() },
                })),
            )
            .await?;
        Ok(status_of(&value))
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-build-storage-request:p1:inst-case-add-file
    }

    async fn indexing_status(
        &self,
        ctx: &SecurityContext,
        target: &StorageTarget,
        vector_store_id: &str,
        provider_file_id: &str,
    ) -> Result<IndexStatus, StorageError> {
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-build-storage-request:p1:inst-case-status-read
        let value = self
            .json_call(
                ctx,
                "GET",
                uri(
                    target,
                    &format!(
                        "/vector_stores/{}/files/{}",
                        segment(vector_store_id),
                        segment(provider_file_id)
                    ),
                ),
                None,
            )
            .await?;
        Ok(status_of(&value))
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-build-storage-request:p1:inst-case-status-read
    }

    async fn delete_file(
        &self,
        ctx: &SecurityContext,
        target: &StorageTarget,
        provider_file_id: &str,
    ) -> Result<(), StorageError> {
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-build-storage-request:p1:inst-case-deletes
        self.delete_call(
            ctx,
            uri(target, &format!("/files/{}", segment(provider_file_id))),
            false,
        )
        .await
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-build-storage-request:p1:inst-case-deletes
    }

    async fn delete_vector_store(
        &self,
        ctx: &SecurityContext,
        target: &StorageTarget,
        vector_store_id: &str,
    ) -> Result<(), StorageError> {
        self.delete_call(
            ctx,
            uri(
                target,
                &format!("/vector_stores/{}", segment(vector_store_id)),
            ),
            false,
        )
        .await
    }

    async fn delete_secondary(
        &self,
        ctx: &SecurityContext,
        alias: &str,
        file_id: &str,
    ) -> Result<(), StorageError> {
        self.delete_call(ctx, format!("/{alias}/v1/files/{}", segment(file_id)), true)
            .await
    }
}
// @cpt-end:cpt-cf-mini-chat-algo-attachments-build-storage-request:p1:inst-match-operation

#[cfg(test)]
#[path = "oagw_storage_tests.rs"]
mod tests;
