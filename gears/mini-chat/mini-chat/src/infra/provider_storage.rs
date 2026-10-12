//! The provider storage gateway over OAGW (ADR-0005 "Storage dispatch"): the Files and Vector
//! Stores APIs of `OpenAI` and Azure `OpenAI`, and the Anthropic Files API for the secondary copy
//! of an image.
//!
//! The storage kind of the provider entry selects the wire flavor: `openai` serves `/v1/...`,
//! `azure` serves `/openai/...?api-version=`. A chat served by an `anthropic_messages` provider
//! stores its files and vector store through the entry named by `rag_provider`. Every request goes
//! through the in-process OAGW client to `/{alias}{path}`; credentials are injected by OAGW.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use futures::StreamExt;
use oagw_sdk::body::BodyStream;
use oagw_sdk::{Body, MultipartBody, Part, ServiceGatewayClientV1};
use serde_json::{Value, json};
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::config::{ProviderEntry, ProviderKind, StorageKind};
use crate::domain::attachment::{
    ChatStorage, FileUpload, IndexingStatus, PROVIDER_KIND_ANTHROPIC, SecondaryRoute, StorageError,
    StorageFlavor, StorageGateway, StorageRoute,
};
use crate::domain::error::DomainError;
use crate::infra::llm::upstream_alias_for_tenant;

/// Largest provider answer that is read into memory.
const MAX_RESPONSE_BYTES: usize = 1024 * 1024;
/// The `purpose` of every uploaded file.
const FILE_PURPOSE: &str = "assistants";
/// Anthropic Files API headers.
const ANTHROPIC_VERSION: (&str, &str) = ("anthropic-version", "2023-06-01");
const ANTHROPIC_BETA: (&str, &str) = ("anthropic-beta", "files-api-2025-04-14");

/// Storage operations of the configured providers.
pub struct OagwStorage {
    gateway: Arc<dyn ServiceGatewayClientV1>,
    entries: HashMap<String, ProviderEntry>,
}

impl OagwStorage {
    /// Creates the gateway over the provider entries.
    #[must_use]
    pub fn new(
        gateway: Arc<dyn ServiceGatewayClientV1>,
        entries: HashMap<String, ProviderEntry>,
    ) -> Self {
        Self { gateway, entries }
    }

    fn entry(&self, provider_id: &str) -> Result<&ProviderEntry, DomainError> {
        self.entries
            .get(provider_id)
            .ok_or_else(|| DomainError::PolicyUnavailable {
                diagnostic: format!("provider `{provider_id}` is not configured"),
            })
    }

    fn alias_of(entry: &ProviderEntry, tenant_id: Uuid) -> String {
        let tenant_override = entry.tenant_overrides.get(&tenant_id.to_string());
        upstream_alias_for_tenant(entry, tenant_override)
    }

    fn route_of(provider_id: &str, entry: &ProviderEntry, tenant_id: Uuid) -> StorageRoute {
        let flavor = match entry.storage_kind {
            StorageKind::Openai => StorageFlavor::Openai,
            StorageKind::Azure => StorageFlavor::Azure {
                api_version: entry.api_version.clone().unwrap_or_default(),
            },
        };
        StorageRoute {
            provider_id: provider_id.to_owned(),
            backend: entry
                .storage_backend
                .clone()
                .unwrap_or_else(|| provider_id.to_owned()),
            alias: Self::alias_of(entry, tenant_id),
            flavor,
        }
    }

    /// Sends a request through the gateway; a non-2xx answer is a [`StorageError`].
    async fn send(
        &self,
        ctx: &SecurityContext,
        request: http::Request<Body>,
    ) -> Result<(http::StatusCode, Bytes), StorageError> {
        let response = self
            .gateway
            .proxy_request(ctx.clone(), request)
            .await
            .map_err(|err| {
                StorageError::gateway(format!("the gateway refused the request: {err}"))
            })?;
        let status = response.status();
        let body = read_capped(response.into_body()).await?;
        Ok((status, body))
    }

    /// Sends a request and requires a 2xx answer with a JSON body.
    async fn send_json(
        &self,
        ctx: &SecurityContext,
        request: http::Request<Body>,
    ) -> Result<Value, StorageError> {
        let (status, body) = self.send(ctx, request).await?;
        if !status.is_success() {
            return Err(StorageError::status(status.as_u16()));
        }
        serde_json::from_slice(&body)
            .map_err(|_| StorageError::malformed("the provider answered with invalid JSON"))
    }

    /// Sends a delete: a 2xx or 404 answer is success.
    async fn send_delete(
        &self,
        ctx: &SecurityContext,
        request: http::Request<Body>,
    ) -> Result<(), StorageError> {
        let (status, _) = self.send(ctx, request).await?;
        if status.is_success() || status == http::StatusCode::NOT_FOUND {
            Ok(())
        } else {
            Err(StorageError::status(status.as_u16()))
        }
    }
}

/// Reads a response body up to [`MAX_RESPONSE_BYTES`].
async fn read_capped(body: Body) -> Result<Bytes, StorageError> {
    let mut stream = body.into_stream();
    let mut collected: Vec<u8> = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk =
            chunk.map_err(|_| StorageError::gateway("the provider response could not be read"))?;
        if collected.len() + chunk.len() > MAX_RESPONSE_BYTES {
            return Err(StorageError::malformed(
                "the provider response is too large",
            ));
        }
        collected.extend_from_slice(&chunk);
    }
    Ok(Bytes::from(collected))
}

/// `/{alias}{prefix}{tail}{?api-version}` of a storage route.
fn storage_uri(route: &StorageRoute, tail: &str) -> String {
    match &route.flavor {
        StorageFlavor::Openai => format!("/{}/v1{tail}", route.alias),
        StorageFlavor::Azure { api_version } => {
            format!("/{}/openai{tail}?api-version={api_version}", route.alias)
        }
    }
}

fn request(
    method: http::Method,
    uri: &str,
    body: Body,
) -> Result<http::Request<Body>, StorageError> {
    http::Request::builder()
        .method(method)
        .uri(uri)
        .header(http::header::ACCEPT, "application/json")
        .body(body)
        .map_err(|_| StorageError::malformed("the provider request could not be built"))
}

fn json_request(
    method: http::Method,
    uri: &str,
    body: &Value,
) -> Result<http::Request<Body>, StorageError> {
    let bytes = serde_json::to_vec(body)
        .map_err(|_| StorageError::malformed("the provider request could not be built"))?;
    let mut built = request(method, uri, Body::Bytes(Bytes::from(bytes)))?;
    built.headers_mut().insert(
        http::header::CONTENT_TYPE,
        http::HeaderValue::from_static("application/json"),
    );
    Ok(built)
}

/// The multipart upload request: `purpose` (when given) and the streamed `file` part.
fn upload_request(
    uri: &str,
    purpose: Option<&str>,
    file: FileUpload,
    headers: &[(&'static str, &'static str)],
) -> Result<http::Request<Body>, StorageError> {
    let chunks = file.chunks;
    let stream: BodyStream = Box::pin(futures::stream::iter(
        chunks
            .into_iter()
            .map(Ok::<Bytes, Box<dyn std::error::Error + Send + Sync>>),
    ));
    let mut multipart = MultipartBody::new();
    if let Some(purpose) = purpose {
        multipart = multipart.text("purpose", purpose);
    }
    multipart = multipart.part(
        Part::stream("file", stream)
            .filename(file.filename)
            .content_type(file.content_type),
    );
    let mut built = multipart
        .into_request(http::Method::POST, uri)
        .map_err(|_| StorageError::malformed("the provider request could not be built"))?;
    built.headers_mut().insert(
        http::header::ACCEPT,
        http::HeaderValue::from_static("application/json"),
    );
    for (name, value) in headers {
        built.headers_mut().insert(
            http::HeaderName::from_static(name),
            http::HeaderValue::from_static(value),
        );
    }
    Ok(built)
}

/// Longest provider identifier or alias that is put into a request path.
const MAX_SEGMENT_CHARS: usize = 256;

/// Whether `value` is safe as one path segment: provider identifiers and upstream aliases are
/// letters, digits and `-_.:` only, so an identifier can never add or climb path segments.
fn is_safe_segment(value: &str) -> bool {
    !value.is_empty()
        && value.chars().count() <= MAX_SEGMENT_CHARS
        && value != "."
        && value != ".."
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | ':'))
}

/// The value as a path segment, or a malformed-input error.
fn segment(value: &str) -> Result<&str, StorageError> {
    if is_safe_segment(value) {
        Ok(value)
    } else {
        Err(StorageError::malformed(
            "an identifier is not usable in a provider path",
        ))
    }
}

/// The `id` member of a provider object; an identifier that cannot be used as a path segment is
/// refused, so it never reaches a later request path.
fn object_id(object: &Value) -> Result<String, StorageError> {
    let id = object
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| StorageError::malformed("the provider answer has no id"))?;
    segment(id).map(str::to_owned)
}

/// The indexing status of a `vector_store.file` object: a missing status counts as
/// `in_progress`, anything but `in_progress` and `completed` as failed.
fn indexing_status(object: &Value) -> IndexingStatus {
    match object.get("status").and_then(Value::as_str) {
        None | Some("in_progress") => IndexingStatus::InProgress,
        Some("completed") => IndexingStatus::Completed,
        Some(other) => IndexingStatus::Failed(other.to_owned()),
    }
}

#[async_trait]
impl StorageGateway for OagwStorage {
    fn chat_storage(&self, tenant_id: Uuid, provider_id: &str) -> Result<ChatStorage, DomainError> {
        let entry = self.entry(provider_id)?;
        let storage_id = entry.rag_provider.as_deref().unwrap_or(provider_id);
        let storage_entry = self.entry(storage_id)?;
        let secondary = (entry.kind == ProviderKind::AnthropicMessages).then(|| SecondaryRoute {
            alias: Self::alias_of(entry, tenant_id),
            provider_kind: PROVIDER_KIND_ANTHROPIC,
        });
        Ok(ChatStorage {
            primary: Self::route_of(storage_id, storage_entry, tenant_id),
            secondary,
        })
    }

    fn route_for_backend(
        &self,
        tenant_id: Uuid,
        backend: &str,
    ) -> Result<StorageRoute, DomainError> {
        // The provider id wins over a `storage_backend` label; labels are matched in id order so
        // that the answer does not depend on the map order.
        let mut ids: Vec<&String> = self.entries.keys().collect();
        ids.sort();
        let found = ids
            .iter()
            .find(|id| id.as_str() == backend)
            .or_else(|| {
                ids.iter().find(|id| {
                    self.entries
                        .get(id.as_str())
                        .and_then(|entry| entry.storage_backend.as_deref())
                        == Some(backend)
                })
            })
            .ok_or_else(|| DomainError::PolicyUnavailable {
                diagnostic: format!("no provider serves the storage backend `{backend}`"),
            })?;
        let entry = self.entry(found)?;
        Ok(Self::route_of(found, entry, tenant_id))
    }

    async fn upload_file(
        &self,
        ctx: &SecurityContext,
        route: &StorageRoute,
        file: FileUpload,
    ) -> Result<String, StorageError> {
        let built = upload_request(&storage_uri(route, "/files"), Some(FILE_PURPOSE), file, &[])?;
        object_id(&self.send_json(ctx, built).await?)
    }

    async fn delete_file(
        &self,
        ctx: &SecurityContext,
        route: &StorageRoute,
        file_id: &str,
    ) -> Result<(), StorageError> {
        let uri = storage_uri(route, &format!("/files/{}", segment(file_id)?));
        self.send_delete(ctx, request(http::Method::DELETE, &uri, Body::Empty)?)
            .await
    }

    async fn create_vector_store(
        &self,
        ctx: &SecurityContext,
        route: &StorageRoute,
    ) -> Result<String, StorageError> {
        let built = json_request(
            http::Method::POST,
            &storage_uri(route, "/vector_stores"),
            &json!({}),
        )?;
        object_id(&self.send_json(ctx, built).await?)
    }

    async fn add_file_to_store(
        &self,
        ctx: &SecurityContext,
        route: &StorageRoute,
        vector_store_id: &str,
        file_id: &str,
        attachment_id: Uuid,
    ) -> Result<IndexingStatus, StorageError> {
        let built = json_request(
            http::Method::POST,
            &storage_uri(
                route,
                &format!("/vector_stores/{}/files", segment(vector_store_id)?),
            ),
            &json!({
                "file_id": file_id,
                "attributes": { "attachment_id": attachment_id.to_string() }
            }),
        )?;
        Ok(indexing_status(&self.send_json(ctx, built).await?))
    }

    async fn file_status(
        &self,
        ctx: &SecurityContext,
        route: &StorageRoute,
        vector_store_id: &str,
        file_id: &str,
    ) -> Result<IndexingStatus, StorageError> {
        let uri = storage_uri(
            route,
            &format!(
                "/vector_stores/{}/files/{}",
                segment(vector_store_id)?,
                segment(file_id)?
            ),
        );
        let built = request(http::Method::GET, &uri, Body::Empty)?;
        Ok(indexing_status(&self.send_json(ctx, built).await?))
    }

    async fn delete_vector_store(
        &self,
        ctx: &SecurityContext,
        route: &StorageRoute,
        vector_store_id: &str,
    ) -> Result<(), StorageError> {
        let uri = storage_uri(
            route,
            &format!("/vector_stores/{}", segment(vector_store_id)?),
        );
        self.send_delete(ctx, request(http::Method::DELETE, &uri, Body::Empty)?)
            .await
    }

    async fn upload_secondary(
        &self,
        ctx: &SecurityContext,
        route: &SecondaryRoute,
        file: FileUpload,
    ) -> Result<String, StorageError> {
        let built = upload_request(
            &format!("/{}/v1/files", segment(&route.alias)?),
            None,
            file,
            &[ANTHROPIC_VERSION, ANTHROPIC_BETA],
        )?;
        object_id(&self.send_json(ctx, built).await?)
    }

    async fn delete_secondary(
        &self,
        ctx: &SecurityContext,
        alias: &str,
        file_id: &str,
    ) -> Result<(), StorageError> {
        let uri = format!("/{}/v1/files/{}", segment(alias)?, segment(file_id)?);
        let mut built = request(http::Method::DELETE, &uri, Body::Empty)?;
        for (name, value) in [ANTHROPIC_VERSION, ANTHROPIC_BETA] {
            built.headers_mut().insert(
                http::HeaderName::from_static(name),
                http::HeaderValue::from_static(value),
            );
        }
        self.send_delete(ctx, built).await
    }
}

#[cfg(test)]
#[path = "provider_storage_tests.rs"]
mod tests;
