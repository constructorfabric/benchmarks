//! Anthropic Files API client: the secondary copy of images uploaded to chats served by an
//! `anthropic_messages` provider (DESIGN "File storage (P1)"). Reached through OAGW with the S2S
//! context at `/{alias}/v1/files`, with the Files API beta header.

use std::sync::Arc;

use bytes::Bytes;
use http::{Method, StatusCode};
use oagw_sdk::{Body, MultipartBody, Part, ServiceGatewayClientV1};

use super::StorageError;
use super::openai::{created_id, invalid_request, path_id, proxy};
use crate::infra::llm::S2sContext;
use crate::infra::llm::anthropic::{ANTHROPIC_VERSION, FILES_API_BETA};

/// Anthropic Files API over the in-process OAGW client.
pub struct AnthropicFiles {
    gateway: Arc<dyn ServiceGatewayClientV1>,
    s2s: S2sContext,
}

impl AnthropicFiles {
    #[must_use]
    pub fn new(gateway: Arc<dyn ServiceGatewayClientV1>, s2s: S2sContext) -> Self {
        Self { gateway, s2s }
    }

    /// Uploads `bytes` as the multipart `file` part (no `purpose`) to the upstream `alias` and
    /// returns the Anthropic file id.
    ///
    /// # Errors
    /// [`StorageError`] when the upload fails or the answer carries no id.
    pub async fn upload(
        &self,
        alias: &str,
        filename: &str,
        content_type: &str,
        bytes: Bytes,
    ) -> Result<String, StorageError> {
        let mut req = MultipartBody::new()
            .part(
                Part::bytes("file", bytes)
                    .filename(filename)
                    .content_type(content_type),
            )
            .into_request(Method::POST, format!("/{alias}/v1/files"))
            .map_err(|_| invalid_request("failed to build the upload request"))?;
        add_headers(&mut req);
        let reply = proxy(&self.gateway, &self.s2s, req).await?;
        created_id(&reply.into_success()?)
    }

    /// Deletes file `file_id` at the upstream `alias`; a missing file counts as deleted.
    ///
    /// # Errors
    /// [`StorageError`] when the delete fails.
    pub async fn delete(&self, alias: &str, file_id: &str) -> Result<(), StorageError> {
        let mut req = http::Request::builder()
            .method(Method::DELETE)
            .uri(format!("/{alias}/v1/files/{}", path_id(file_id)?))
            .header(http::header::ACCEPT, "application/json")
            .body(Body::Empty)
            .map_err(|_| invalid_request("failed to build the delete request"))?;
        add_headers(&mut req);
        let reply = proxy(&self.gateway, &self.s2s, req).await?;
        if reply.status == StatusCode::NOT_FOUND && !reply.is_gateway_error() {
            return Ok(());
        }
        reply.into_success().map(drop)
    }
}

fn add_headers(req: &mut http::Request<Body>) {
    let headers = req.headers_mut();
    headers.insert(
        "anthropic-version",
        http::HeaderValue::from_static(ANTHROPIC_VERSION),
    );
    headers.insert(
        "anthropic-beta",
        http::HeaderValue::from_static(FILES_API_BETA),
    );
}
