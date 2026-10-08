//! Transport to providers through OAGW (`ServiceGatewayClientV1::proxy_request`),
//! provider resolution and the Files / Vector Stores client.

use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use oagw_sdk::{Body, MultipartBody, Part, ServiceGatewayClientV1};
use parking_lot::RwLock;
use serde_json::{Value, json};
use toolkit_canonical_errors::CanonicalError;
use toolkit_security::SecurityContext;
use toolkit_security::constants::{DEFAULT_SUBJECT_ID, DEFAULT_TENANT_ID};
use uuid::Uuid;

use crate::config::{MiniChatConfig, ProviderKind, StorageKind};

/// Sends proxied HTTP requests to providers.
#[async_trait]
pub trait ProviderTransport: Send + Sync {
    /// # Errors
    /// Gateway-originated failure.
    async fn send(&self, req: http::Request<Body>) -> Result<http::Response<Body>, CanonicalError>;
}

/// Holder of the S2S security context obtained at gear start.
#[derive(Default)]
pub struct S2sContext {
    ctx: RwLock<Option<SecurityContext>>,
}

impl S2sContext {
    pub fn set(&self, ctx: SecurityContext) {
        *self.ctx.write() = Some(ctx);
    }

    #[must_use]
    pub fn get(&self) -> SecurityContext {
        if let Some(c) = self.ctx.read().clone() {
            return c;
        }
        SecurityContext::builder()
            .subject_id(DEFAULT_SUBJECT_ID)
            .subject_tenant_id(DEFAULT_TENANT_ID)
            .token_scopes(vec!["*".to_owned()])
            .build()
            .unwrap_or_else(|_| SecurityContext::anonymous())
    }
}

/// Configured alias → alias registered in OAGW (when OAGW derived another one).
#[derive(Default)]
pub struct AliasMap {
    map: RwLock<std::collections::HashMap<String, String>>,
}

impl AliasMap {
    pub fn insert(&self, configured: &str, actual: &str) {
        if configured != actual {
            self.map.write().insert(configured.to_owned(), actual.to_owned());
        }
    }

    /// Rewrites the alias segment of a proxy URI.
    #[must_use]
    pub fn rewrite(&self, uri: &http::Uri) -> Option<http::Uri> {
        let path = uri.path();
        let rest = path.strip_prefix('/')?;
        let (alias, tail) = rest.split_once('/').unwrap_or((rest, ""));
        let actual = self.map.read().get(alias).cloned()?;
        let q = uri.query().map(|q| format!("?{q}")).unwrap_or_default();
        format!("/{actual}/{tail}{q}").parse().ok()
    }
}

/// Production transport: in-process OAGW proxy under the S2S identity.
pub struct OagwTransport {
    pub gateway: Arc<dyn ServiceGatewayClientV1>,
    pub s2s: Arc<S2sContext>,
    pub aliases: Arc<AliasMap>,
}

#[async_trait]
impl ProviderTransport for OagwTransport {
    async fn send(&self, mut req: http::Request<Body>) -> Result<http::Response<Body>, CanonicalError> {
        if let Some(uri) = self.aliases.rewrite(req.uri()) {
            *req.uri_mut() = uri;
        }
        self.gateway.proxy_request(self.s2s.get(), req).await
    }
}

/// A chat provider resolved for a tenant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedProvider {
    pub provider_id: String,
    pub kind: ProviderKind,
    pub alias: String,
    pub api_path: String,
}

impl ResolvedProvider {
    /// Proxy URI of the chat endpoint for a provider model.
    #[must_use]
    pub fn chat_uri(&self, provider_model_id: &str) -> String {
        format!(
            "/{}{}",
            self.alias,
            self.api_path.replace("{model}", provider_model_id)
        )
    }
}

fn alias_for(cfg: &MiniChatConfig, provider_id: &str, tenant: Uuid) -> Option<String> {
    let p = cfg.providers.get(provider_id)?;
    if let Some(o) = p.tenant_overrides.get(&tenant.to_string()) {
        if let Some(a) = o.upstream_alias.as_ref().filter(|a| !a.is_empty()) {
            return Some(a.clone());
        }
        if let Some(h) = o.host.as_ref().filter(|h| !h.is_empty()) {
            return Some(h.clone());
        }
    }
    Some(p.alias())
}

/// Resolves a chat provider for a tenant.
#[must_use]
pub fn resolve_provider(cfg: &MiniChatConfig, provider_id: &str, tenant: Uuid) -> Option<ResolvedProvider> {
    let p = cfg.providers.get(provider_id)?;
    Some(ResolvedProvider {
        provider_id: provider_id.to_owned(),
        kind: p.kind,
        alias: alias_for(cfg, provider_id, tenant)?,
        api_path: p.api_path.clone(),
    })
}

/// File / vector store target of a provider for a tenant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StorageTarget {
    pub provider_id: String,
    pub storage_kind: StorageKind,
    pub alias: String,
    pub api_version: Option<String>,
    pub backend_label: String,
}

impl StorageTarget {
    fn prefix(&self) -> &'static str {
        match self.storage_kind {
            StorageKind::Openai => "/v1",
            StorageKind::Azure => "/openai",
        }
    }

    /// Proxy URI of a RAG path (`/files`, `/vector_stores/...`).
    #[must_use]
    pub fn uri(&self, path: &str) -> String {
        let base = format!("/{}{}{}", self.alias, self.prefix(), path);
        match (self.storage_kind, &self.api_version) {
            (StorageKind::Azure, Some(v)) => format!("{base}?api-version={v}"),
            _ => base,
        }
    }
}

/// Storage target used for models served by `provider_id` (`rag_provider`, else itself).
#[must_use]
pub fn resolve_storage(cfg: &MiniChatConfig, provider_id: &str, tenant: Uuid) -> Option<StorageTarget> {
    let sid = cfg.storage_provider_id(provider_id).to_owned();
    let p = cfg.providers.get(&sid)?;
    Some(StorageTarget {
        storage_kind: p.storage_kind?,
        alias: alias_for(cfg, &sid, tenant)?,
        api_version: p.api_version.clone(),
        backend_label: cfg.storage_backend_label(&sid),
        provider_id: sid,
    })
}

/// Storage target from an attachment's stored backend label.
#[must_use]
pub fn storage_from_label(cfg: &MiniChatConfig, label: &str, tenant: Uuid) -> Option<StorageTarget> {
    let pid = cfg.provider_for_storage_label(label)?.to_owned();
    let p = cfg.providers.get(&pid)?;
    Some(StorageTarget {
        storage_kind: p.storage_kind?,
        alias: alias_for(cfg, &pid, tenant)?,
        api_version: p.api_version.clone(),
        backend_label: label.to_owned(),
        provider_id: pid,
    })
}

/// Failure of a storage call.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("storage error (status {status:?}, transient {transient}): {message}")]
pub struct StorageError {
    pub status: Option<u16>,
    pub transient: bool,
    pub message: String,
}

impl StorageError {
    fn gateway(err: &CanonicalError) -> Self {
        Self {
            status: None,
            transient: true,
            message: err.to_string(),
        }
    }
}

/// Files / Vector Stores API client over a transport.
#[derive(Clone)]
pub struct StorageClient {
    pub transport: Arc<dyn ProviderTransport>,
}

async fn read_json(resp: http::Response<Body>) -> Result<(u16, Value), StorageError> {
    let status = resp.status().as_u16();
    let bytes = resp
        .into_body()
        .into_bytes()
        .await
        .map_err(|e| StorageError {
            status: Some(status),
            transient: true,
            message: e.to_string(),
        })?;
    let v = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    Ok((status, v))
}

fn status_error(status: u16, v: &Value) -> StorageError {
    StorageError {
        status: Some(status),
        transient: status >= 500 || status == 429,
        message: super::error_message_from_body(&serde_json::to_vec(v).unwrap_or_default()),
    }
}

fn json_request(method: &str, uri: String, body: &Value) -> Result<http::Request<Body>, StorageError> {
    http::Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .body(Body::Bytes(Bytes::from(serde_json::to_vec(body).unwrap_or_default())))
        .map_err(|e| StorageError {
            status: None,
            transient: false,
            message: e.to_string(),
        })
}

fn empty_request(method: &str, uri: String) -> Result<http::Request<Body>, StorageError> {
    http::Request::builder()
        .method(method)
        .uri(uri)
        .body(Body::Empty)
        .map_err(|e| StorageError {
            status: None,
            transient: false,
            message: e.to_string(),
        })
}

impl StorageClient {
    async fn call(&self, req: http::Request<Body>) -> Result<(u16, Value), StorageError> {
        let resp = self
            .transport
            .send(req)
            .await
            .map_err(|e| StorageError::gateway(&e))?;
        read_json(resp).await
    }

    /// Uploads a file (`purpose=assistants` unless `with_purpose` is false).
    ///
    /// # Errors
    /// Gateway or provider failure.
    pub async fn upload_file(
        &self,
        target: &StorageTarget,
        filename: &str,
        content_type: &str,
        data: Bytes,
        with_purpose: bool,
    ) -> Result<String, StorageError> {
        let mut mp = MultipartBody::new();
        if with_purpose {
            mp = mp.text("purpose", "assistants");
        }
        mp = mp.part(
            Part::bytes("file", data)
                .filename(filename.to_owned())
                .content_type(content_type.to_owned()),
        );
        let req = mp
            .into_request("POST", target.uri("/files"))
            .map_err(|e| StorageError {
                status: None,
                transient: false,
                message: e.to_string(),
            })?;
        let (status, v) = self.call(req).await?;
        if !(200..300).contains(&status) {
            return Err(status_error(status, &v));
        }
        v.get("id")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| StorageError {
                status: Some(status),
                transient: false,
                message: "file upload response has no id".to_owned(),
            })
    }

    /// Deletes a file; 2xx and 404 are success.
    ///
    /// # Errors
    /// Any other status or gateway failure.
    pub async fn delete_file(&self, target: &StorageTarget, file_id: &str) -> Result<(), StorageError> {
        let req = empty_request("DELETE", target.uri(&format!("/files/{file_id}")))?;
        let (status, v) = self.call(req).await?;
        if (200..300).contains(&status) || status == 404 {
            Ok(())
        } else {
            Err(status_error(status, &v))
        }
    }

    /// Creates a vector store.
    ///
    /// # Errors
    /// Gateway or provider failure.
    pub async fn create_vector_store(&self, target: &StorageTarget, name: &str) -> Result<String, StorageError> {
        let req = json_request("POST", target.uri("/vector_stores"), &json!({ "name": name }))?;
        let (status, v) = self.call(req).await?;
        if !(200..300).contains(&status) {
            return Err(status_error(status, &v));
        }
        v.get("id")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| StorageError {
                status: Some(status),
                transient: false,
                message: "vector store response has no id".to_owned(),
            })
    }

    /// Adds a file to a vector store; returns the reported status.
    ///
    /// # Errors
    /// Gateway or provider failure.
    pub async fn add_vector_store_file(
        &self,
        target: &StorageTarget,
        vector_store_id: &str,
        file_id: &str,
        attachment_id: Uuid,
    ) -> Result<Option<String>, StorageError> {
        let req = json_request(
            "POST",
            target.uri(&format!("/vector_stores/{vector_store_id}/files")),
            &json!({ "file_id": file_id, "attributes": { "attachment_id": attachment_id.to_string() } }),
        )?;
        let (status, v) = self.call(req).await?;
        if !(200..300).contains(&status) {
            return Err(status_error(status, &v));
        }
        Ok(v.get("status").and_then(Value::as_str).map(str::to_owned))
    }

    /// Reads the indexing status of a vector store file.
    ///
    /// # Errors
    /// Gateway or provider failure (`transient` for 5xx / gateway errors).
    pub async fn vector_store_file_status(
        &self,
        target: &StorageTarget,
        vector_store_id: &str,
        file_id: &str,
    ) -> Result<Option<String>, StorageError> {
        let req = empty_request(
            "GET",
            target.uri(&format!("/vector_stores/{vector_store_id}/files/{file_id}")),
        )?;
        let (status, v) = self.call(req).await?;
        if !(200..300).contains(&status) {
            return Err(status_error(status, &v));
        }
        Ok(v.get("status").and_then(Value::as_str).map(str::to_owned))
    }

    /// Deletes a vector store; 2xx and 404 are success.
    ///
    /// # Errors
    /// Any other status or gateway failure.
    pub async fn delete_vector_store(&self, target: &StorageTarget, vector_store_id: &str) -> Result<(), StorageError> {
        let req = empty_request("DELETE", target.uri(&format!("/vector_stores/{vector_store_id}")))?;
        let (status, v) = self.call(req).await?;
        if (200..300).contains(&status) || status == 404 {
            Ok(())
        } else {
            Err(status_error(status, &v))
        }
    }
}
