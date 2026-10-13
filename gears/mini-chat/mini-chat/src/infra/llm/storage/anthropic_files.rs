//! Anthropic Files API client for the secondary copies of images in
//! Anthropic chats (ADR-0005, D "Files API upload field mapping").
//!
//! Requests go to `/{alias}/v1/files` of the Anthropic upstream with the
//! `anthropic-version` header; the upload carries only the `file` part.

use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use http::Method;
use oagw_sdk::{MultipartBody, Part, ServiceGatewayClientV1};

use super::{Payload, RagStorage, path_id};
use crate::config::StorageKind;
use crate::domain::ports::{SecondaryFilesPort, StorageError};
use crate::infra::llm::provider_resolver::StorageTarget;
use crate::infra::llm::providers::anthropic_messages::ANTHROPIC_VERSION;
use crate::infra::s2s::S2sContextProvider;

const VERSION_HEADER: (&str, &str) = ("anthropic-version", ANTHROPIC_VERSION);

/// Anthropic Files API client.
pub struct AnthropicFiles {
    inner: RagStorage,
}

impl AnthropicFiles {
    #[must_use]
    pub fn new(oagw: Arc<dyn ServiceGatewayClientV1>, s2s: Arc<S2sContextProvider>) -> Self {
        Self {
            inner: RagStorage::new(oagw, s2s),
        }
    }

    /// `/{alias}/v1/...` target of the Anthropic upstream.
    fn target(alias: &str) -> StorageTarget {
        StorageTarget {
            provider_id: "anthropic".to_owned(),
            storage_kind: StorageKind::Openai,
            alias: alias.to_owned(),
            api_version: None,
            storage_backend: String::new(),
        }
    }
}

#[async_trait]
impl SecondaryFilesPort for AnthropicFiles {
    async fn upload(
        &self,
        alias: &str,
        filename: &str,
        content_type: &str,
        bytes: Bytes,
    ) -> Result<String, StorageError> {
        let form = MultipartBody::new().part(
            Part::bytes("file", bytes)
                .filename(filename)
                .content_type(content_type),
        );
        let json = self
            .inner
            .call_with_headers(
                "anthropic file upload",
                &Self::target(alias),
                Method::POST,
                "/files",
                Payload::Multipart(form),
                false,
                &[VERSION_HEADER],
            )
            .await?;
        json.get("id")
            .and_then(serde_json::Value::as_str)
            .filter(|id| !id.is_empty())
            .map(ToOwned::to_owned)
            .ok_or_else(|| {
                StorageError::Permanent("anthropic file upload: response has no file id".into())
            })
    }

    async fn delete(&self, alias: &str, file_id: &str) -> Result<(), StorageError> {
        let path = format!("/files/{}", path_id("file", file_id)?);
        self.inner
            .call_with_headers(
                "anthropic file delete",
                &Self::target(alias),
                Method::DELETE,
                &path,
                Payload::Empty,
                true,
                &[VERSION_HEADER],
            )
            .await
            .map(drop)
    }
}
