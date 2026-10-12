//! Provider Files and Vector Stores operations (DESIGN 3.5 "Provider API
//! Mapping", 3.2 `llm_provider` storage side) over the in-process OAGW proxy
//! client: no credential header is set, OAGW injects the provider credentials.
//!
//! The provider entry is resolved **by the storage label**
//! (`providers.<id>.storage_backend`, default the provider id); a catalog
//! `provider_id` reaches its entry through `rag_provider`. The wire shape
//! follows `storage_kind`: `/v1/...` for `openai`, `/openai/...` plus the
//! `api-version` query for `azure`. A 2xx answer is success; a `404` on a
//! delete is success; every other status and every gateway failure is an error
//! (transient for a provider 5xx / 429 and for gateway failures).
//!
//! Provider answers are only parsed for the fields the gear needs; their text is
//! never kept in an error.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use async_trait::async_trait;
use authn_resolver_sdk::AuthNResolverClient;
use bytes::Bytes;
use futures::StreamExt;
use http::header::{ACCEPT, CONTENT_TYPE};
use oagw_sdk::body::Body;
use oagw_sdk::{MultipartBody, Part, ServiceGatewayClientV1};
use serde_json::{Value, json};
use toolkit_security::SecurityContext;
use uuid::Uuid;

use super::openai_responses::encode_path_segment;
use super::providers::ServiceIdentity;
use crate::config::{ClientCredentialsConfig, MiniChatConfig, StorageKind};
use crate::domain::error::DomainError;
use crate::domain::ports::{FileUpload, IndexStatus, StorageError, StoragePort, StorageTarget};

/// Largest provider answer read (the answers are small JSON objects).
const BODY_LIMIT: usize = 64 * 1024;

/// One provider entry as the storage side sees it.
struct Entry {
    kind: StorageKind,
    api_version: Option<String>,
    /// Alias of the entry's own upstream.
    alias: String,
    /// Alias per tenant override.
    tenant_aliases: HashMap<Uuid, String>,
}

impl Entry {
    fn alias_for(&self, tenant_id: Uuid) -> &str {
        self.tenant_aliases
            .get(&tenant_id)
            .map_or(self.alias.as_str(), String::as_str)
    }

    /// `/{alias}{prefix}{path}` with the Azure `api-version` query.
    fn uri(&self, tenant_id: Uuid, path: &str) -> String {
        let alias = self.alias_for(tenant_id);
        match self.kind {
            StorageKind::OpenAi => format!("/{alias}/v1{path}"),
            StorageKind::Azure => {
                let version = self.api_version.as_deref().unwrap_or_default();
                format!("/{alias}/openai{path}?api-version={version}")
            }
        }
    }
}

/// The storage side of the provider layer.
pub struct ProviderStorage {
    oagw: Arc<dyn ServiceGatewayClientV1>,
    /// Entry per storage label.
    by_label: HashMap<String, Entry>,
    /// Storage label that serves the files of a catalog provider id.
    label_of_provider: HashMap<String, String>,
    identity: Option<ServiceIdentity>,
}

impl ProviderStorage {
    /// Builds the resolver from the provider entries.
    #[must_use]
    pub fn new(oagw: Arc<dyn ServiceGatewayClientV1>, config: &MiniChatConfig) -> Self {
        // Sorted, so that two entries sharing a label resolve deterministically.
        let sorted: BTreeMap<&str, _> = config
            .providers
            .iter()
            .map(|(id, provider)| (id.as_str(), provider))
            .collect();
        let label_of = |id: &str| {
            sorted
                .get(id)
                .and_then(|provider| provider.storage_backend.clone())
                .unwrap_or_else(|| id.to_owned())
        };
        let mut by_label = HashMap::new();
        for (id, provider) in &sorted {
            let id: &str = id;
            let tenant_aliases = provider
                .tenant_overrides
                .iter()
                .filter_map(|(tenant, tenant_override)| {
                    let tenant_id = Uuid::parse_str(tenant).ok()?;
                    Some((tenant_id, provider.tenant_target(tenant_override).alias))
                })
                .collect();
            by_label.entry(label_of(id)).or_insert(Entry {
                kind: provider.storage_kind,
                api_version: provider.api_version.clone(),
                alias: provider.target().alias,
                tenant_aliases,
            });
        }
        let label_of_provider = sorted
            .iter()
            .map(|(id, provider)| {
                let serving = provider.rag_provider.as_deref().unwrap_or(id);
                ((*id).to_owned(), label_of(serving))
            })
            .collect();
        Self {
            oagw,
            by_label,
            label_of_provider,
            identity: None,
        }
    }

    /// Makes storage calls run as the gear's own S2S identity (the one that
    /// registered the upstreams).
    #[must_use]
    pub fn with_service_identity(
        mut self,
        authn: Arc<dyn AuthNResolverClient>,
        credentials: &ClientCredentialsConfig,
    ) -> Self {
        self.identity = Some(ServiceIdentity::new(authn, credentials));
        self
    }

    fn entry(&self, target: &StorageTarget) -> Result<&Entry, StorageError> {
        self.by_label
            .get(&target.backend)
            .ok_or_else(|| StorageError {
                transient: false,
                message: "unknown storage backend".to_owned(),
            })
    }

    async fn call_context(
        &self,
        caller: &SecurityContext,
    ) -> Result<SecurityContext, StorageError> {
        match &self.identity {
            Some(identity) => identity.context().await.map_err(|detail| {
                tracing::error!(%detail, "service identity is not available for the storage call");
                StorageError {
                    transient: true,
                    message: "service identity unavailable".to_owned(),
                }
            }),
            None => Ok(caller.clone()),
        }
    }

    /// Sends the request and returns the status and the (bounded) answer body.
    async fn send(
        &self,
        ctx: &SecurityContext,
        what: &str,
        request: http::Request<Body>,
    ) -> Result<(http::StatusCode, Bytes), StorageError> {
        let call_ctx = self.call_context(ctx).await?;
        let response = self
            .oagw
            .proxy_request(call_ctx, request)
            .await
            .map_err(|error| {
                tracing::warn!(
                    category = error.gts_type(),
                    detail = error.detail(),
                    "OAGW storage call failed: {what}"
                );
                StorageError {
                    transient: true,
                    message: format!("{what}: gateway failure"),
                }
            })?;
        let status = response.status();
        let body = read_limited(response.into_body()).await;
        Ok((status, body))
    }

    /// A request without a body.
    fn empty(method: http::Method, uri: &str) -> Result<http::Request<Body>, StorageError> {
        http::Request::builder()
            .method(method)
            .uri(uri)
            .header(ACCEPT, "application/json")
            .body(Body::from(()))
            .map_err(|error| build_error(&error))
    }

    /// A JSON request.
    fn json(uri: &str, body: &Value) -> Result<http::Request<Body>, StorageError> {
        let bytes = serde_json::to_vec(body).map_err(|error| StorageError {
            transient: false,
            message: format!("request body: {error}"),
        })?;
        http::Request::builder()
            .method(http::Method::POST)
            .uri(uri)
            .header(CONTENT_TYPE, "application/json")
            .header(ACCEPT, "application/json")
            .body(Body::from(bytes))
            .map_err(|error| build_error(&error))
    }

    /// A delete: 2xx and 404 are success.
    async fn delete(
        &self,
        ctx: &SecurityContext,
        what: &str,
        uri: &str,
    ) -> Result<(), StorageError> {
        let (status, _) = self
            .send(ctx, what, Self::empty(http::Method::DELETE, uri)?)
            .await?;
        if status.is_success() || status == http::StatusCode::NOT_FOUND {
            Ok(())
        } else {
            Err(status_error(what, status))
        }
    }
}

fn build_error(error: &http::Error) -> StorageError {
    StorageError {
        transient: false,
        message: format!("request does not build: {error}"),
    }
}

fn status_error(what: &str, status: http::StatusCode) -> StorageError {
    StorageError {
        transient: status.is_server_error() || status == http::StatusCode::TOO_MANY_REQUESTS,
        message: format!("{what}: HTTP {}", status.as_u16()),
    }
}

async fn read_limited(body: Body) -> Bytes {
    let mut stream = body.into_stream();
    let mut buffer = Vec::new();
    while let Some(chunk) = stream.next().await {
        let Ok(chunk) = chunk else { break };
        buffer.extend_from_slice(&chunk);
        if buffer.len() >= BODY_LIMIT {
            break;
        }
    }
    Bytes::from(buffer)
}

/// The `id` of a created provider object.
fn id_of(what: &str, body: &[u8]) -> Result<String, StorageError> {
    serde_json::from_slice::<Value>(body)
        .ok()
        .and_then(|value| value.get("id").and_then(Value::as_str).map(str::to_owned))
        .filter(|id| !id.is_empty())
        .ok_or_else(|| StorageError {
            transient: false,
            message: format!("{what}: the answer has no id"),
        })
}

/// The indexing status of a vector store file answer: `completed`, `in_progress`
/// or a missing status (keep waiting), anything else is a failure.
fn index_status_of(body: &[u8]) -> IndexStatus {
    let status = serde_json::from_slice::<Value>(body)
        .ok()
        .and_then(|value| {
            value
                .get("status")
                .and_then(Value::as_str)
                .map(str::to_owned)
        });
    match status.as_deref() {
        Some("completed") => IndexStatus::Completed,
        None | Some("in_progress") => IndexStatus::InProgress,
        Some(_) => IndexStatus::Failed,
    }
}

#[async_trait]
impl StoragePort for ProviderStorage {
    fn backend_for(&self, provider_id: &str) -> Result<String, DomainError> {
        self.label_of_provider
            .get(provider_id)
            .cloned()
            .ok_or_else(|| {
                DomainError::internal(format!("provider `{provider_id}` is not configured"))
            })
    }

    fn upstream_alias(&self, target: &StorageTarget) -> Option<String> {
        self.entry(target)
            .ok()
            .map(|entry| entry.alias_for(target.tenant_id).to_owned())
    }

    async fn upload_file(
        &self,
        ctx: &SecurityContext,
        target: &StorageTarget,
        file: FileUpload,
    ) -> Result<String, StorageError> {
        let uri = self.entry(target)?.uri(target.tenant_id, "/files");
        let request = MultipartBody::new()
            .text("purpose", "assistants")
            .part(
                Part::bytes("file", file.data)
                    .filename(file.filename)
                    .content_type(file.content_type),
            )
            .into_request(http::Method::POST, uri)
            .map_err(|error| build_error(&error))?;
        let (status, body) = self.send(ctx, "upload file", request).await?;
        if !status.is_success() {
            return Err(status_error("upload file", status));
        }
        id_of("upload file", &body)
    }

    async fn delete_file(
        &self,
        ctx: &SecurityContext,
        target: &StorageTarget,
        file_id: &str,
    ) -> Result<(), StorageError> {
        let path = format!("/files/{}", encode_path_segment(file_id));
        let uri = self.entry(target)?.uri(target.tenant_id, &path);
        self.delete(ctx, "delete file", &uri).await
    }

    async fn create_vector_store(
        &self,
        ctx: &SecurityContext,
        target: &StorageTarget,
    ) -> Result<String, StorageError> {
        let uri = self.entry(target)?.uri(target.tenant_id, "/vector_stores");
        let request = Self::json(&uri, &json!({}))?;
        let (status, body) = self.send(ctx, "create vector store", request).await?;
        if !status.is_success() {
            return Err(status_error("create vector store", status));
        }
        id_of("create vector store", &body)
    }

    async fn add_file_to_store(
        &self,
        ctx: &SecurityContext,
        target: &StorageTarget,
        store_id: &str,
        file_id: &str,
        attachment_id: Uuid,
    ) -> Result<IndexStatus, StorageError> {
        let path = format!("/vector_stores/{}/files", encode_path_segment(store_id));
        let uri = self.entry(target)?.uri(target.tenant_id, &path);
        let request = Self::json(
            &uri,
            &json!({
                "file_id": file_id,
                "attributes": {"attachment_id": attachment_id.to_string()},
            }),
        )?;
        let (status, body) = self.send(ctx, "add file to vector store", request).await?;
        if !status.is_success() {
            return Err(status_error("add file to vector store", status));
        }
        Ok(index_status_of(&body))
    }

    async fn index_status(
        &self,
        ctx: &SecurityContext,
        target: &StorageTarget,
        store_id: &str,
        file_id: &str,
    ) -> Result<IndexStatus, StorageError> {
        let path = format!(
            "/vector_stores/{}/files/{}",
            encode_path_segment(store_id),
            encode_path_segment(file_id)
        );
        let uri = self.entry(target)?.uri(target.tenant_id, &path);
        let (status, body) = self
            .send(
                ctx,
                "read vector store file",
                Self::empty(http::Method::GET, &uri)?,
            )
            .await?;
        if !status.is_success() {
            return Err(status_error("read vector store file", status));
        }
        Ok(index_status_of(&body))
    }

    async fn delete_vector_store(
        &self,
        ctx: &SecurityContext,
        target: &StorageTarget,
        store_id: &str,
    ) -> Result<(), StorageError> {
        let path = format!("/vector_stores/{}", encode_path_segment(store_id));
        let uri = self.entry(target)?.uri(target.tenant_id, &path);
        self.delete(ctx, "delete vector store", &uri).await
    }
}
