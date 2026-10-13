//! RAG storage client (spec §11.7): provider Files API and Vector Stores API
//! through the OAGW in-process proxy, with the gear's S2S security context.
//!
//! `storage_kind = openai` uses `/{alias}/v1/...`; `azure` uses
//! `/{alias}/openai/...?api-version={api_version}`. A provider `404` on a
//! delete is success. Failures are split into [`StorageError::Transient`]
//! (gateway failure, provider `429` / `408` / 5xx) and [`StorageError::Failed`]
//! (any other provider answer); the text is for logs only.

use std::sync::Arc;

use bytes::Bytes;
use http::{Method, StatusCode, header};
use oagw_sdk::api::ErrorSource;
use oagw_sdk::{Body, MultipartBody, Part, ServiceGatewayClientV1};
use serde_json::{Value, json};
use uuid::Uuid;

use super::ResolvedProvider;
use crate::config::StorageKind;
use crate::infra::oagw::provisioning::Provisioner;
use crate::infra::oagw::s2s::S2sContext;

/// `purpose` of every uploaded file.
pub const FILE_PURPOSE: &str = "assistants";

/// Indexing state of a file in a vector store.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VsFileStatus {
    /// `in_progress`, or a response without `status`.
    InProgress,
    Completed,
    /// `failed`, `cancelled` or any other value.
    Failed,
}

impl VsFileStatus {
    /// Status of a vector-store file object.
    #[must_use]
    pub fn from_response(body: &Value) -> Self {
        match body.get("status").and_then(Value::as_str) {
            None | Some("in_progress") => Self::InProgress,
            Some("completed") => Self::Completed,
            Some(_) => Self::Failed,
        }
    }
}

/// A failed storage call.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum StorageError {
    /// Gateway failure, provider rate limit, timeout or 5xx: worth retrying.
    #[error("transient storage failure: {0}")]
    Transient(String),
    /// Any other provider answer.
    #[error("storage failure: {0}")]
    Failed(String),
}

impl StorageError {
    #[must_use]
    pub fn is_transient(&self) -> bool {
        matches!(self, Self::Transient(_))
    }
}

/// Files / vector stores client over OAGW.
pub struct RagClient {
    gw: Arc<dyn ServiceGatewayClientV1>,
    s2s: Arc<S2sContext>,
    /// Provisions a deferred provider on demand before a request.
    provisioner: Option<Arc<Provisioner>>,
}

impl RagClient {
    #[must_use]
    pub fn new(gw: Arc<dyn ServiceGatewayClientV1>, s2s: Arc<S2sContext>) -> Self {
        Self {
            gw,
            s2s,
            provisioner: None,
        }
    }

    /// Provision a still-deferred provider (rate-limited) before its requests.
    #[must_use]
    pub fn with_provisioner(mut self, provisioner: Arc<Provisioner>) -> Self {
        self.provisioner = Some(provisioner);
        self
    }

    /// Upload `bytes` as `filename` (`purpose = assistants`); returns the provider file id.
    ///
    /// # Errors
    /// Gateway or provider failure, or a response without a file id.
    pub async fn upload_file(
        &self,
        p: &ResolvedProvider,
        filename: &str,
        content_type: &str,
        bytes: Bytes,
    ) -> Result<String, StorageError> {
        let refreshed = self.refresh(p).await;
        let p = refreshed.as_ref().unwrap_or(p);
        let body = MultipartBody::new().text("purpose", FILE_PURPOSE).part(
            Part::bytes("file", bytes)
                .filename(filename)
                .content_type(content_type),
        );
        let req = body
            .into_request(Method::POST, storage_uri(p, "/files"))
            .map_err(|e| StorageError::Failed(format!("cannot build upload request: {e}")))?;
        let resp = self.send(req).await?;
        id_of(&resp, "file")
    }

    /// Delete a provider file (`404` is success).
    ///
    /// # Errors
    /// Gateway or provider failure.
    pub async fn delete_file(
        &self,
        p: &ResolvedProvider,
        file_id: &str,
    ) -> Result<(), StorageError> {
        let refreshed = self.refresh(p).await;
        let p = refreshed.as_ref().unwrap_or(p);
        self.delete(&storage_uri(p, &format!("/files/{file_id}")))
            .await
    }

    /// Create the vector store `name`; returns its provider id.
    ///
    /// # Errors
    /// Gateway or provider failure, or a response without an id.
    pub async fn create_vector_store(
        &self,
        p: &ResolvedProvider,
        name: &str,
    ) -> Result<String, StorageError> {
        let refreshed = self.refresh(p).await;
        let p = refreshed.as_ref().unwrap_or(p);
        let resp = self
            .send_json(
                Method::POST,
                &storage_uri(p, "/vector_stores"),
                &json!({ "name": name }),
            )
            .await?;
        id_of(&resp, "vector store")
    }

    /// Add `file_id` to the vector store `vs` with the `attachment_id` attribute;
    /// returns the indexing status of the answer.
    ///
    /// # Errors
    /// Gateway or provider failure.
    pub async fn add_file(
        &self,
        p: &ResolvedProvider,
        vs: &str,
        file_id: &str,
        attachment_id: Uuid,
    ) -> Result<VsFileStatus, StorageError> {
        let refreshed = self.refresh(p).await;
        let p = refreshed.as_ref().unwrap_or(p);
        let resp = self
            .send_json(
                Method::POST,
                &storage_uri(p, &format!("/vector_stores/{vs}/files")),
                &json!({
                    "file_id": file_id,
                    "attributes": { "attachment_id": attachment_id.to_string() },
                }),
            )
            .await?;
        Ok(VsFileStatus::from_response(&resp))
    }

    /// Indexing status of `file_id` in the vector store `vs`.
    ///
    /// # Errors
    /// Gateway or provider failure.
    pub async fn file_status(
        &self,
        p: &ResolvedProvider,
        vs: &str,
        file_id: &str,
    ) -> Result<VsFileStatus, StorageError> {
        let refreshed = self.refresh(p).await;
        let p = refreshed.as_ref().unwrap_or(p);
        let req = request(
            Method::GET,
            &storage_uri(p, &format!("/vector_stores/{vs}/files/{file_id}")),
            Body::Empty,
            None,
        )?;
        let resp = self.send(req).await?;
        Ok(VsFileStatus::from_response(&resp))
    }

    /// Delete the vector store `vs` (`404` is success).
    ///
    /// # Errors
    /// Gateway or provider failure.
    pub async fn delete_vector_store(
        &self,
        p: &ResolvedProvider,
        vs: &str,
    ) -> Result<(), StorageError> {
        let refreshed = self.refresh(p).await;
        let p = refreshed.as_ref().unwrap_or(p);
        self.delete(&storage_uri(p, &format!("/vector_stores/{vs}")))
            .await
    }

    /// Proxy the request `build` makes for `p` (after an on-demand provisioning
    /// attempt, which may change the alias); the JSON body of a 2xx answer.
    ///
    /// # Errors
    /// `build` failure, gateway or provider failure (`404` is `Failed`).
    pub(crate) async fn call(
        &self,
        p: &ResolvedProvider,
        build: impl FnOnce(&ResolvedProvider) -> Result<http::Request<Body>, StorageError>,
    ) -> Result<Value, StorageError> {
        let refreshed = self.refresh(p).await;
        let req = build(refreshed.as_ref().unwrap_or(p))?;
        Ok(self.send(req).await?)
    }

    /// Like [`Self::call`] for a delete: a provider `404` is success.
    ///
    /// # Errors
    /// `build` failure, gateway or provider failure.
    pub(crate) async fn call_delete(
        &self,
        p: &ResolvedProvider,
        build: impl FnOnce(&ResolvedProvider) -> Result<http::Request<Body>, StorageError>,
    ) -> Result<(), StorageError> {
        let refreshed = self.refresh(p).await;
        let req = build(refreshed.as_ref().unwrap_or(p))?;
        match self.send(req).await {
            Ok(_) | Err(Outcome::NotFound) => Ok(()),
            Err(Outcome::Error(e)) => Err(e),
        }
    }

    /// `p` re-resolved after an on-demand provisioning attempt of its
    /// still-deferred provider.
    async fn refresh(&self, p: &ResolvedProvider) -> Option<ResolvedProvider> {
        match &self.provisioner {
            Some(provisioner) => provisioner.ensure_provisioned(p).await,
            None => None,
        }
    }

    async fn send_json(
        &self,
        method: Method,
        uri: &str,
        body: &Value,
    ) -> Result<Value, StorageError> {
        let req = request(
            method,
            uri,
            Body::from(body.to_string()),
            Some("application/json"),
        )?;
        Ok(self.send(req).await?)
    }

    async fn delete(&self, uri: &str) -> Result<(), StorageError> {
        let req = request(Method::DELETE, uri, Body::Empty, None)?;
        match self.send(req).await {
            Ok(_) | Err(Outcome::NotFound) => Ok(()),
            Err(Outcome::Error(e)) => Err(e),
        }
    }

    /// Proxy `req`; the JSON body of a 2xx answer (`Null` when not JSON).
    async fn send(&self, req: http::Request<Body>) -> Result<Value, Outcome> {
        let ctx = self
            .s2s
            .get()
            .map_err(|e| Outcome::Error(StorageError::Transient(e.to_string())))?;
        let resp =
            self.gw.proxy_request(ctx, req).await.map_err(|e| {
                Outcome::Error(StorageError::Transient(format!("gateway error: {e}")))
            })?;
        let status = resp.status();
        let source = resp
            .extensions()
            .get::<ErrorSource>()
            .copied()
            .unwrap_or(ErrorSource::Upstream);
        let body = resp
            .into_body()
            .into_bytes()
            .await
            .map_err(|e| Outcome::Error(StorageError::Transient(format!("read body: {e}"))))?;
        if status.is_success() {
            return Ok(serde_json::from_slice(&body).unwrap_or(Value::Null));
        }
        let text = String::from_utf8_lossy(&body);
        if source == ErrorSource::Gateway {
            return Err(Outcome::Error(StorageError::Transient(format!(
                "gateway answered HTTP {}: {text}",
                status.as_u16()
            ))));
        }
        if status == StatusCode::NOT_FOUND {
            return Err(Outcome::NotFound);
        }
        let msg = format!("provider answered HTTP {}: {text}", status.as_u16());
        Err(Outcome::Error(
            if status.is_server_error()
                || status == StatusCode::TOO_MANY_REQUESTS
                || status == StatusCode::REQUEST_TIMEOUT
            {
                StorageError::Transient(msg)
            } else {
                StorageError::Failed(msg)
            },
        ))
    }
}

/// Result of a proxied call that did not succeed.
enum Outcome {
    /// Provider `404 Not Found`.
    NotFound,
    Error(StorageError),
}

impl From<Outcome> for StorageError {
    fn from(o: Outcome) -> Self {
        match o {
            Outcome::NotFound => Self::Failed("provider answered HTTP 404".to_owned()),
            Outcome::Error(e) => e,
        }
    }
}

pub(crate) fn request(
    method: Method,
    uri: &str,
    body: Body,
    content_type: Option<&'static str>,
) -> Result<http::Request<Body>, StorageError> {
    let mut b = http::Request::builder().method(method).uri(uri);
    if let Some(ct) = content_type {
        b = b.header(header::CONTENT_TYPE, ct);
    }
    b.body(body)
        .map_err(|e| StorageError::Failed(format!("cannot build storage request: {e}")))
}

/// The `id` of a created object.
pub(crate) fn id_of(resp: &Value, what: &str) -> Result<String, StorageError> {
    resp.get("id")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| StorageError::Failed(format!("{what} response has no id")))
}

/// `proxy_request` URI of the storage `path` (`/files`, `/vector_stores/...`).
#[must_use]
pub fn storage_uri(p: &ResolvedProvider, path: &str) -> String {
    match p.storage_kind {
        Some(StorageKind::Azure) => {
            let mut uri = format!("/{}/openai{path}", p.alias);
            if let Some(v) = &p.api_version {
                uri.push_str("?api-version=");
                uri.push_str(v);
            }
            uri
        }
        Some(StorageKind::Openai) | None => format!("/{}/v1{path}", p.alias),
    }
}

#[cfg(test)]
#[path = "storage_tests.rs"]
mod storage_tests;
