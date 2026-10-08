//! Anthropic Files API client.
//!
//! Uploads files to the Anthropic Files API (`POST /v1/files`) and
//! deletes them (`DELETE /v1/files/{file_id}`). Both paths go through
//! OAGW as a transparent proxy.
//!
//! Beta header required: `anthropic-beta: files-api-2025-04-14`
//! (`anthropic-version` is injected by OAGW per the upstream headers config —
//! see [`super::anthropic_messages::upstream_headers`].)

use std::sync::Arc;

use bytes::Bytes;
use oagw_sdk::{MultipartBody, Part, ServiceGatewayClientV1};
use serde::Deserialize;
use toolkit_security::SecurityContext;

use crate::infra::llm::LlmProviderError;

/// Uploaded file reference returned by the Files API.
#[derive(Debug, Clone)]
pub struct AnthropicFileRef {
    pub file_id: String,
}

/// Response from POST /v1/files
#[derive(Deserialize)]
struct UploadResponse {
    id: String,
}

/// Anthropic Files API client.
pub struct AnthropicFilesClient {
    gateway: Arc<dyn ServiceGatewayClientV1>,
}

impl AnthropicFilesClient {
    #[must_use]
    pub fn new(gateway: Arc<dyn ServiceGatewayClientV1>) -> Self {
        Self { gateway }
    }

    /// Upload a file to the Anthropic Files API.
    ///
    /// `filename` is used as the multipart form field name.
    /// `content_type` is the MIME type (e.g., `"application/pdf"`, `"image/png"`).
    pub async fn upload_file(
        &self,
        ctx: SecurityContext,
        upstream_alias: &str,
        filename: &str,
        content_type: &str,
        data: Bytes,
    ) -> Result<AnthropicFileRef, LlmProviderError> {
        // OAGW expects `/{upstream_alias}/{route_path}`. The Anthropic Files
        // API route is registered at `POST /v1/files` (see
        // `oagw_provisioning::register_rag_routes`). Without the path
        // suffix OAGW finds no matching route and returns 404 — and the
        // attachment ends up with `anthropic_status=failed`.
        let uri = format!("/{upstream_alias}/v1/files");

        let multipart = MultipartBody::new().part(
            Part::bytes("file", data)
                .filename(filename.to_owned())
                .content_type(content_type.to_owned()),
        );
        let http_request = multipart
            .into_request(http::Method::POST, &uri)
            .map_err(|e| LlmProviderError::InvalidResponse {
                detail: format!("failed to build upload request: {e}"),
            })?
            .into_parts();
        let (mut parts, body) = http_request;
        parts.headers.insert(
            "anthropic-beta",
            http::HeaderValue::from_static(super::anthropic_messages::ANTHROPIC_FILES_BETA),
        );
        let http_request = http::Request::from_parts(parts, body);

        let response = self.gateway.proxy_request(ctx, http_request).await?;
        let (parts, body) = response.into_parts();
        let bytes = body
            .into_bytes()
            .await
            .map_err(|e| LlmProviderError::InvalidResponse {
                detail: format!("failed to read upload response: {e}"),
            })?;

        if !parts.status.is_success() {
            return Err(super::anthropic_messages::parse_anthropic_error(
                parts.status,
                &parts.headers,
                &bytes,
            ));
        }

        let resp: UploadResponse =
            serde_json::from_slice(&bytes).map_err(|e| LlmProviderError::InvalidResponse {
                detail: format!("failed to parse upload response: {e}"),
            })?;

        Ok(AnthropicFileRef { file_id: resp.id })
    }

    /// Delete a file from the Anthropic Files API.
    ///
    /// Anthropic's wire path is `DELETE /v1/files/{file_id}`. The OAGW route
    /// registered in `oagw_provisioning::register_rag_routes` for DELETE on
    /// `/v1/files` uses `PathSuffixMode::Append`, so OAGW matches the full
    /// `/{upstream_alias}/v1/files/{file_id}` URI and forwards the suffix.
    ///
    /// A 404 from Anthropic is treated as success — the file is already gone
    /// (idempotency), mirroring how the primary `RagHttpClient::delete`
    /// handles missing files.
    pub async fn delete_file(
        &self,
        ctx: SecurityContext,
        upstream_alias: &str,
        file_id: &str,
    ) -> Result<(), LlmProviderError> {
        let uri = format!("/{upstream_alias}/v1/files/{file_id}");

        let http_request = http::Request::builder()
            .method(http::Method::DELETE)
            .uri(&uri)
            .header(
                "anthropic-beta",
                super::anthropic_messages::ANTHROPIC_FILES_BETA,
            )
            .body(oagw_sdk::Body::Bytes(Bytes::new()))
            .map_err(|e| LlmProviderError::InvalidResponse {
                detail: format!("failed to build delete request: {e}"),
            })?;

        let response = self.gateway.proxy_request(ctx, http_request).await?;
        let (parts, body) = response.into_parts();

        if parts.status == http::StatusCode::NOT_FOUND {
            return Ok(());
        }
        if !parts.status.is_success() {
            let bytes = body
                .into_bytes()
                .await
                .map_err(|e| LlmProviderError::InvalidResponse {
                    detail: format!("failed to read delete error body: {e}"),
                })?;
            return Err(super::anthropic_messages::parse_anthropic_error(
                parts.status,
                &parts.headers,
                &bytes,
            ));
        }

        Ok(())
    }
}
