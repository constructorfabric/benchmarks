//! RAG storage clients: provider Files API and Vector Stores API through the
//! OAGW proxy (DESIGN section 3.5 "LLM Provider" route table and "Provider
//! API Mapping", section 3.6 "File Upload", ADR-0005, ADR-0007).
//!
//! [`ProviderStorage`] implements [`FileStorage`] and [`VectorStores`] for
//! every configured provider entry. A call names its storage backend label
//! ([`StorageTarget::backend`], stored in `attachments.storage_backend` and
//! `chat_vector_stores.provider`); the label is mapped back to the provider
//! entry that carries it (the entry whose id equals the label first, else
//! the first entry in id order), and the entry's `storage_kind` selects the
//! API flavour:
//!
//! | `storage_kind` | Path | Query |
//! |---|---|---|
//! | `openai` | `/{alias}/v1/{resource}` | — |
//! | `azure` | `/{alias}/openai/{resource}` | `api-version={api_version}` |
//!
//! The alias is the tenant override's alias when the tenant has one, else
//! the entry's `upstream_alias`. Requests carry the gear's S2S security
//! context (the upstreams are registered under it). Uploads are
//! `multipart/form-data` with `purpose=assistants` and the `file` part;
//! vector store files are added with the single attribute
//! `{attachment_id}`.
//!
//! Outcome classification ([`StorageError`]): 2xx is success (and `404 Not
//! Found` for deletes); a gateway failure, a missing S2S context, an
//! unreadable response body and provider 408 / 429 / 5xx are transient;
//! every other status, and a success body without the expected fields, is
//! rejected. Error texts are diagnostics only.

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use http::{Method, Request, Response, StatusCode, header};
use oagw_sdk::api::ServiceGatewayClientV1;
use oagw_sdk::body::Body;
use oagw_sdk::{MultipartBody, Part, ServiceGatewayError};
use serde_json::{Value, json};
use tracing::debug;
use uuid::Uuid;

use super::openai_responses::encode_path_segment;
use super::resolver::tenant_override;
use crate::config::{ProviderEntry, StorageKind};
use crate::domain::error::DomainError;
use crate::domain::ports::{
    FileStorage, FileUpload, StorageError, StorageTarget, VectorStoreFileStatus, VectorStores,
};
use crate::infra::oagw_provisioning::SystemContextSource;

/// `purpose` of every Files API upload (documents and images).
pub const FILE_PURPOSE: &str = "assistants";

/// Error diagnostics are cut to this many characters.
const DIAGNOSTIC_MAX_CHARS: usize = 300;

/// Where one storage call goes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StorageEndpoint {
    /// Provider entry the backend label resolved to.
    pub provider_id: String,
    /// OAGW alias for the tenant.
    pub alias: String,
    pub kind: StorageKind,
    /// Azure `api-version`.
    pub api_version: Option<String>,
}

impl StorageEndpoint {
    /// Proxy URI of `resource` (a path below the API prefix, segments already
    /// encoded).
    ///
    /// # Errors
    ///
    /// [`StorageError::Rejected`] for an Azure entry without `api_version`.
    pub fn uri(&self, resource: &str) -> Result<String, StorageError> {
        match self.kind {
            StorageKind::Openai => Ok(format!("/{}/v1/{resource}", self.alias)),
            StorageKind::Azure => {
                let version = self
                    .api_version
                    .as_deref()
                    .filter(|version| !version.trim().is_empty())
                    .ok_or_else(|| {
                        StorageError::Rejected(format!(
                            "provider '{}' has no api_version for RAG requests",
                            self.provider_id
                        ))
                    })?;
                Ok(format!(
                    "/{}/openai/{resource}?api-version={version}",
                    self.alias
                ))
            }
        }
    }
}

/// File and vector-store implementation over the configured providers.
pub struct ProviderStorage {
    gateway: Arc<dyn ServiceGatewayClientV1>,
    system: Arc<dyn SystemContextSource>,
    providers: BTreeMap<String, ProviderEntry>,
}

impl std::fmt::Debug for ProviderStorage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProviderStorage")
            .field("providers", &self.providers.keys().collect::<Vec<_>>())
            .finish_non_exhaustive()
    }
}

/// What a storage call expects from the response.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Expect {
    /// A JSON body.
    Json,
    /// A deletion: 2xx or `404 Not Found`, the body is ignored.
    Deleted,
}

impl ProviderStorage {
    /// Storage over the OAGW client, the S2S context source and the prepared
    /// provider entries (upstream aliases filled).
    #[must_use]
    pub fn new(
        gateway: Arc<dyn ServiceGatewayClientV1>,
        system: Arc<dyn SystemContextSource>,
        providers: &BTreeMap<String, ProviderEntry>,
    ) -> Self {
        Self {
            gateway,
            system,
            providers: providers.clone(),
        }
    }

    /// The endpoint of `target`: the provider entry carrying the backend
    /// label and the tenant's alias.
    ///
    /// # Errors
    ///
    /// [`StorageError::Rejected`] when no entry carries the label.
    pub fn endpoint(&self, target: &StorageTarget) -> Result<StorageEndpoint, StorageError> {
        let label = target.backend.as_str();
        let (provider_id, entry) = self
            .providers
            .get_key_value(label)
            .filter(|(id, entry)| entry.effective_storage_backend(id) == label)
            .or_else(|| {
                self.providers
                    .iter()
                    .find(|(id, entry)| entry.effective_storage_backend(id) == label)
            })
            .ok_or_else(|| {
                StorageError::Rejected(format!("no provider carries storage backend '{label}'"))
            })?;
        let alias = tenant_override(entry, target.tenant_id)
            .and_then(|o| o.upstream_alias.as_deref().or(o.host.as_deref()))
            .or(entry.upstream_alias.as_deref())
            .unwrap_or(&entry.host);
        Ok(StorageEndpoint {
            provider_id: provider_id.clone(),
            alias: alias.to_owned(),
            kind: entry.storage_kind,
            api_version: entry.api_version.clone(),
        })
    }

    /// Sends `request` with the S2S context and classifies the outcome.
    async fn send(
        &self,
        endpoint: &StorageEndpoint,
        request: Request<Body>,
        expect: Expect,
    ) -> Result<Value, StorageError> {
        let ctx =
            self.system.system_context().await.map_err(|err| {
                StorageError::Transient(format!("S2S context unavailable: {err}"))
            })?;
        let operation = format!("{} {}", request.method(), request.uri().path());
        let response = self
            .gateway
            .proxy_request(ctx, request)
            .await
            .map_err(|err| {
                StorageError::Transient(format!(
                    "gateway failure: {}",
                    ServiceGatewayError::from(err)
                ))
            })?;
        let result = classify(response, expect).await;
        if let Err(err) = &result {
            debug!(
                target: "mini_chat.storage",
                provider = %endpoint.provider_id,
                operation = %operation,
                error = %err,
                "storage call failed"
            );
        }
        result
    }

    async fn json_call(
        &self,
        target: &StorageTarget,
        method: Method,
        resource: &str,
        body: Option<&Value>,
        expect: Expect,
    ) -> Result<Value, StorageError> {
        let endpoint = self.endpoint(target)?;
        let uri = endpoint.uri(resource)?;
        let builder = Request::builder().method(method).uri(uri);
        let request = match body {
            Some(body) => {
                let bytes = serde_json::to_vec(body)
                    .map_err(|err| StorageError::Rejected(format!("request body: {err}")))?;
                builder
                    .header(header::CONTENT_TYPE, "application/json")
                    .header(header::ACCEPT, "application/json")
                    .body(Body::from(bytes))
            }
            None => builder
                .header(header::ACCEPT, "application/json")
                .body(Body::Empty),
        }
        .map_err(|err| StorageError::Rejected(format!("request build: {err}")))?;
        self.send(&endpoint, request, expect).await
    }
}

/// Classifies `response`: the JSON body of a success, `Null` for a
/// deletion, a [`StorageError`] otherwise.
pub(crate) async fn classify(
    response: Response<Body>,
    expect: Expect,
) -> Result<Value, StorageError> {
    let status = response.status();
    if expect == Expect::Deleted && (status.is_success() || status == StatusCode::NOT_FOUND) {
        return Ok(Value::Null);
    }
    let bytes = response
        .into_body()
        .into_bytes()
        .await
        .map_err(|err| StorageError::Transient(format!("response body unreadable: {err}")))?;
    if status.is_success() {
        return serde_json::from_slice(&bytes).map_err(|err| {
            StorageError::Rejected(format!(
                "HTTP {}: invalid JSON body: {err}",
                status.as_u16()
            ))
        });
    }
    let message = format!(
        "HTTP {}: {}",
        status.as_u16(),
        error_message(&bytes).unwrap_or_default()
    );
    if is_transient_status(status) {
        Err(StorageError::Transient(message))
    } else {
        Err(StorageError::Rejected(message))
    }
}

/// Provider 408, 429 and 5xx may succeed on a later call.
#[must_use]
pub fn is_transient_status(status: StatusCode) -> bool {
    status.is_server_error()
        || status == StatusCode::TOO_MANY_REQUESTS
        || status == StatusCode::REQUEST_TIMEOUT
}

/// `error.message`, `message` or `detail` of an error body, cut to a bounded
/// length.
fn error_message(body: &[u8]) -> Option<String> {
    let value: Value = serde_json::from_slice(body).ok()?;
    let message = value
        .get("error")
        .and_then(|error| error.get("message").or(Some(error)))
        .and_then(Value::as_str)
        .or_else(|| value.get("message").and_then(Value::as_str))
        .or_else(|| value.get("detail").and_then(Value::as_str))?;
    Some(message.chars().take(DIAGNOSTIC_MAX_CHARS).collect())
}

/// The `id` of a created provider object.
pub(crate) fn created_id(value: &Value, what: &str) -> Result<String, StorageError> {
    value
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| StorageError::Rejected(format!("{what} response has no id")))
}

/// Indexing status of a vector store file object: a missing `status` is
/// `in_progress`.
#[must_use]
pub fn parse_file_status(value: &Value) -> VectorStoreFileStatus {
    match value.get("status").and_then(Value::as_str) {
        None | Some("in_progress") => VectorStoreFileStatus::InProgress,
        Some("completed") => VectorStoreFileStatus::Completed,
        Some("failed") => VectorStoreFileStatus::Failed,
        Some("cancelled") => VectorStoreFileStatus::Cancelled,
        Some(other) => VectorStoreFileStatus::Other(other.to_owned()),
    }
}

#[async_trait]
impl FileStorage for ProviderStorage {
    fn backend_for_provider(&self, provider_id: &str) -> Result<String, DomainError> {
        let entry = self.providers.get(provider_id).ok_or_else(|| {
            DomainError::internal(format!(
                "model provider '{provider_id}' is not configured in mini-chat providers"
            ))
        })?;
        let rag_id = entry.rag_provider.as_deref().unwrap_or(provider_id);
        let rag_entry = self.providers.get(rag_id).ok_or_else(|| {
            DomainError::internal(format!(
                "rag_provider '{rag_id}' of provider '{provider_id}' is not configured"
            ))
        })?;
        Ok(rag_entry.effective_storage_backend(rag_id).to_owned())
    }

    async fn upload_file(
        &self,
        target: &StorageTarget,
        file: FileUpload,
    ) -> Result<String, StorageError> {
        let endpoint = self.endpoint(target)?;
        let uri = endpoint.uri("files")?;
        let request = MultipartBody::new()
            .text("purpose", FILE_PURPOSE)
            .part(
                Part::bytes("file", file.data)
                    .filename(file.filename)
                    .content_type(file.content_type),
            )
            .into_request(Method::POST, uri)
            .map_err(|err| StorageError::Rejected(format!("request build: {err}")))?;
        let value = self.send(&endpoint, request, Expect::Json).await?;
        created_id(&value, "file upload")
    }

    async fn delete_file(&self, target: &StorageTarget, file_id: &str) -> Result<(), StorageError> {
        let resource = format!("files/{}", encode_path_segment(file_id));
        self.json_call(target, Method::DELETE, &resource, None, Expect::Deleted)
            .await
            .map(drop)
    }
}

#[async_trait]
impl VectorStores for ProviderStorage {
    async fn create_vector_store(
        &self,
        target: &StorageTarget,
        chat_id: Uuid,
    ) -> Result<String, StorageError> {
        let body = json!({ "name": format!("mini-chat {chat_id}") });
        let value = self
            .json_call(
                target,
                Method::POST,
                "vector_stores",
                Some(&body),
                Expect::Json,
            )
            .await?;
        created_id(&value, "vector store create")
    }

    async fn delete_vector_store(
        &self,
        target: &StorageTarget,
        vector_store_id: &str,
    ) -> Result<(), StorageError> {
        let resource = format!("vector_stores/{}", encode_path_segment(vector_store_id));
        self.json_call(target, Method::DELETE, &resource, None, Expect::Deleted)
            .await
            .map(drop)
    }

    async fn add_file(
        &self,
        target: &StorageTarget,
        vector_store_id: &str,
        file_id: &str,
        attachment_id: Uuid,
    ) -> Result<VectorStoreFileStatus, StorageError> {
        let resource = format!(
            "vector_stores/{}/files",
            encode_path_segment(vector_store_id)
        );
        let body = json!({
            "file_id": file_id,
            "attributes": { "attachment_id": attachment_id.to_string() },
        });
        let value = self
            .json_call(target, Method::POST, &resource, Some(&body), Expect::Json)
            .await?;
        Ok(parse_file_status(&value))
    }

    async fn file_status(
        &self,
        target: &StorageTarget,
        vector_store_id: &str,
        file_id: &str,
    ) -> Result<VectorStoreFileStatus, StorageError> {
        let resource = format!(
            "vector_stores/{}/files/{}",
            encode_path_segment(vector_store_id),
            encode_path_segment(file_id)
        );
        let value = self
            .json_call(target, Method::GET, &resource, None, Expect::Json)
            .await?;
        Ok(parse_file_status(&value))
    }
}

#[cfg(test)]
#[path = "storage_tests.rs"]
mod tests;
