//! Anthropic Files API client: secondary copies of images (DESIGN section
//! 3.2 `llm_provider`, section 3.6 "File Upload", section 3.7
//! `attachments.secondary_*`, "Attachment Deletion", B.9.2, ADR-0005).
//!
//! Created only when at least one provider entry uses `anthropic_messages`
//! ([`AnthropicFiles::from_providers`]). A chat whose model is served by such
//! an entry gets a copy of each uploaded image in that entry's Files API:
//! `POST /{alias}/v1/files` with only the `file` part (no `purpose`); the copy
//! is deleted with `DELETE /{alias}/v1/files/{file_id}` (2xx and `404 Not
//! Found` are success). Every call carries the `anthropic-version` header and
//! the Files API beta, and the gear's S2S security context (the upstreams are
//! registered under it); OAGW injects the credential. The alias is the tenant
//! override's alias when the tenant has one, else the entry's
//! `upstream_alias`. A delete resolves the alias from the entry the copy was
//! uploaded with (`attachments.secondary_provider_id`); an entry that is no
//! longer configured resolves to nothing (the delete is skipped rather than
//! sent to another account). Only a row stored without the entry falls back
//! to the kind (`secondary_provider_kind = anthropic`): the first
//! `anthropic_messages` entry in id order, with a warning when several
//! entries make that choice ambiguous.
//!
//! Outcomes are classified like the RAG storage calls
//! ([`super::storage`]): a gateway failure, a missing S2S context and
//! provider 408 / 429 / 5xx are transient, any other non-2xx status is
//! rejected.

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use http::{HeaderValue, Method, Request};
use oagw_sdk::api::ServiceGatewayClientV1;
use oagw_sdk::body::Body;
use oagw_sdk::{MultipartBody, Part, ServiceGatewayError};
use serde_json::Value;
use tracing::{debug, warn};
use uuid::Uuid;

use super::anthropic_messages::{
    ANTHROPIC_BETA_HEADER, ANTHROPIC_VERSION, ANTHROPIC_VERSION_HEADER, FILES_API_BETA,
};
use super::openai_responses::encode_path_segment;
use super::resolver::tenant_alias;
use super::storage::{Expect, classify, created_id};
use crate::config::{ProviderEntry, ProviderKind};
use crate::domain::ports::{
    FileUpload, SECONDARY_KIND_ANTHROPIC, SecondaryFileStorage, SecondaryFileUploader,
    SecondaryTarget, StorageError,
};
use crate::infra::chat_cleanup_handler::SecondaryAliasResolver;
use crate::infra::oagw_provisioning::SystemContextSource;

/// Anthropic Files API client over the `anthropic_messages` provider entries.
pub struct AnthropicFiles {
    gateway: Arc<dyn ServiceGatewayClientV1>,
    system: Arc<dyn SystemContextSource>,
    /// The `anthropic_messages` entries (upstream aliases filled).
    providers: BTreeMap<String, ProviderEntry>,
}

impl std::fmt::Debug for AnthropicFiles {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AnthropicFiles")
            .field("providers", &self.providers.keys().collect::<Vec<_>>())
            .finish_non_exhaustive()
    }
}

impl AnthropicFiles {
    /// The client over the `anthropic_messages` entries of `providers`;
    /// `None` when there is none.
    #[must_use]
    pub fn from_providers(
        gateway: Arc<dyn ServiceGatewayClientV1>,
        system: Arc<dyn SystemContextSource>,
        providers: &BTreeMap<String, ProviderEntry>,
    ) -> Option<Self> {
        let providers: BTreeMap<String, ProviderEntry> = providers
            .iter()
            .filter(|(_, entry)| entry.kind == ProviderKind::AnthropicMessages)
            .map(|(id, entry)| (id.clone(), entry.clone()))
            .collect();
        (!providers.is_empty()).then_some(Self {
            gateway,
            system,
            providers,
        })
    }

    /// Proxy URI of `resource` below `/v1` of `alias`.
    fn uri(alias: &str, resource: &str) -> String {
        format!("/{alias}/v1/{resource}")
    }

    /// Adds the version and Files API beta headers.
    fn with_headers(mut request: Request<Body>) -> Request<Body> {
        let headers = request.headers_mut();
        headers.insert(
            ANTHROPIC_VERSION_HEADER,
            HeaderValue::from_static(ANTHROPIC_VERSION),
        );
        headers.insert(
            ANTHROPIC_BETA_HEADER,
            HeaderValue::from_static(FILES_API_BETA),
        );
        request
    }

    /// Sends `request` with the S2S context and classifies the outcome.
    async fn send(&self, request: Request<Body>, expect: Expect) -> Result<Value, StorageError> {
        let ctx =
            self.system.system_context().await.map_err(|err| {
                StorageError::Transient(format!("S2S context unavailable: {err}"))
            })?;
        let operation = format!("{} {}", request.method(), request.uri().path());
        let response = self
            .gateway
            .proxy_request(ctx, Self::with_headers(request))
            .await
            .map_err(|err| {
                StorageError::Transient(format!(
                    "gateway failure: {}",
                    ServiceGatewayError::from(err)
                ))
            })?;
        let result = classify(response, expect).await;
        if let Err(err) = &result {
            debug!(target: "mini_chat.storage", operation = %operation, error = %err, "Anthropic Files API call failed");
        }
        result
    }

    /// The alias of a stored copy: from the entry it was uploaded with, by
    /// kind only when the row does not name one (module docs).
    fn stored_alias(
        &self,
        tenant_id: Uuid,
        provider_kind: &str,
        provider_id: Option<&str>,
    ) -> Option<String> {
        if provider_kind != SECONDARY_KIND_ANTHROPIC {
            return None;
        }
        if let Some(provider_id) = provider_id {
            let entry = self.providers.get(provider_id);
            if entry.is_none() {
                warn!(target: "mini_chat.storage", provider_id = %provider_id, "secondary copy provider entry is not configured; its upstream alias cannot be resolved");
            }
            return entry.map(|entry| tenant_alias(entry, tenant_id).to_owned());
        }
        let (first_id, entry) = self.providers.iter().next()?;
        if self.providers.len() > 1 {
            warn!(target: "mini_chat.storage", provider_id = %first_id, entries = self.providers.len(), "secondary copy without a stored provider entry; several anthropic_messages entries make the alias ambiguous, using the first");
        }
        Some(tenant_alias(entry, tenant_id).to_owned())
    }
}

#[async_trait]
impl SecondaryFileStorage for AnthropicFiles {
    async fn delete_secondary_file(
        &self,
        _tenant_id: Uuid,
        file_id: &str,
        provider_kind: &str,
        upstream_alias: &str,
    ) -> Result<(), StorageError> {
        if provider_kind != SECONDARY_KIND_ANTHROPIC {
            return Err(StorageError::Rejected(format!(
                "unsupported secondary provider kind '{provider_kind}'"
            )));
        }
        let uri = Self::uri(
            upstream_alias,
            &format!("files/{}", encode_path_segment(file_id)),
        );
        let request = Request::builder()
            .method(Method::DELETE)
            .uri(uri)
            .header(http::header::ACCEPT, "application/json")
            .body(Body::Empty)
            .map_err(|err| StorageError::Rejected(format!("request build: {err}")))?;
        self.send(request, Expect::Deleted).await.map(drop)
    }
}

#[async_trait]
impl SecondaryFileUploader for AnthropicFiles {
    fn secondary_target(&self, provider_id: &str, tenant_id: Uuid) -> Option<SecondaryTarget> {
        let entry = self.providers.get(provider_id)?;
        Some(SecondaryTarget {
            provider_id: provider_id.to_owned(),
            provider_kind: SECONDARY_KIND_ANTHROPIC.to_owned(),
            upstream_alias: tenant_alias(entry, tenant_id).to_owned(),
        })
    }

    async fn upload_secondary_file(
        &self,
        target: &SecondaryTarget,
        file: FileUpload,
    ) -> Result<String, StorageError> {
        let request = MultipartBody::new()
            .part(
                Part::bytes("file", file.data)
                    .filename(file.filename)
                    .content_type(file.content_type),
            )
            .into_request(Method::POST, Self::uri(&target.upstream_alias, "files"))
            .map_err(|err| StorageError::Rejected(format!("request build: {err}")))?;
        let value = self.send(request, Expect::Json).await?;
        created_id(&value, "secondary file upload")
    }
}

impl SecondaryAliasResolver for AnthropicFiles {
    fn upstream_alias(
        &self,
        tenant_id: Uuid,
        provider_kind: &str,
        provider_id: Option<&str>,
    ) -> Option<String> {
        self.stored_alias(tenant_id, provider_kind, provider_id)
    }
}

#[cfg(test)]
#[path = "anthropic_files_tests.rs"]
mod tests;
