//! Anthropic Files API client for the secondary image copies of Anthropic
//! chats (DESIGN §2.2 "File storage (P1)", §3.2 `llm_provider`, §4 "Attachment
//! Deletion" `secondary_ref`).
//!
//! `POST /{alias}/v1/files` (multipart with only the `file` part, no
//! `purpose`) and `DELETE /{alias}/v1/files/{id}` through the OAGW proxy with
//! the gear's S2S context, the `anthropic-version` header and the Files API
//! beta header. Created only when an `anthropic_messages` entry exists.

use std::sync::Arc;

use bytes::Bytes;
use http::{HeaderValue, Method};
use oagw_sdk::{Body, MultipartBody, Part};

use super::anthropic_messages::{ANTHROPIC_VERSION, FILES_API_BETA};
use super::storage::{id_of, request};
use super::{RagClient, ResolvedProvider, StorageError};

/// Value of `attachments.secondary_provider_kind` (and `secondary_ref.provider_kind`).
pub const SECONDARY_PROVIDER_KIND: &str = "anthropic";

/// Anthropic Files API over OAGW.
pub struct AnthropicFilesClient {
    rag: Arc<RagClient>,
}

fn files_uri(p: &ResolvedProvider, path: &str) -> String {
    format!("/{}/v1/files{path}", p.alias)
}

fn with_headers(mut req: http::Request<Body>) -> http::Request<Body> {
    let h = req.headers_mut();
    h.insert(
        "anthropic-version",
        HeaderValue::from_static(ANTHROPIC_VERSION),
    );
    h.insert("anthropic-beta", HeaderValue::from_static(FILES_API_BETA));
    req
}

impl AnthropicFilesClient {
    /// Client sending through `rag`'s gateway, S2S context and provisioner.
    #[must_use]
    pub fn new(rag: Arc<RagClient>) -> Self {
        Self { rag }
    }

    /// Upload `bytes` as `filename` to the Anthropic provider `p`; returns its file id.
    ///
    /// # Errors
    /// Gateway or provider failure, or a response without an id.
    pub async fn upload(
        &self,
        p: &ResolvedProvider,
        filename: &str,
        content_type: &str,
        bytes: Bytes,
    ) -> Result<String, StorageError> {
        let resp = self
            .rag
            .call(p, |p| {
                MultipartBody::new()
                    .part(
                        Part::bytes("file", bytes)
                            .filename(filename)
                            .content_type(content_type),
                    )
                    .into_request(Method::POST, files_uri(p, ""))
                    .map(with_headers)
                    .map_err(|e| StorageError::Failed(format!("cannot build upload request: {e}")))
            })
            .await?;
        id_of(&resp, "anthropic file")
    }

    /// Delete the Anthropic file `file_id` (`404` is success).
    ///
    /// # Errors
    /// Gateway or provider failure.
    pub async fn delete(&self, p: &ResolvedProvider, file_id: &str) -> Result<(), StorageError> {
        self.rag
            .call_delete(p, |p| {
                request(
                    Method::DELETE,
                    &files_uri(p, &format!("/{file_id}")),
                    Body::Empty,
                    None,
                )
                .map(with_headers)
            })
            .await
    }
}
