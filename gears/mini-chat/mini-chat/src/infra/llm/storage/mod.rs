//! File / vector-store clients behind the [`RagStorage`] port, sent through OAGW.
//!
//! Paths (DESIGN section 3.5, RAG routes): `openai` uses `/{alias}/v1/...`,
//! `azure` uses `/{alias}/openai/...?api-version=V`. Failures are classified as
//! [`StorageError::Unavailable`] (not sent: S2S identity not ready),
//! [`StorageError::Transient`] (gateway errors, provider 5xx / 429),
//! [`StorageError::NotFound`] (provider 404) or [`StorageError::Failed`] (any
//! other 4xx, malformed answers, missing configuration).

pub mod azure;
pub mod openai;

use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use oagw_sdk::{Body, MultipartBody, Part, ServiceGatewayClientV1};
use serde_json::{Value, json};
use toolkit_canonical_errors::CanonicalError;
use uuid::Uuid;

use super::ServiceIdentity;
use super::providers::from_gateway;
use super::types::ResolvedStorage;
use crate::config::StorageKind;
use crate::domain::ports::{IndexStatus, RagStorage, StorageError};
use crate::domain::sanitize::sanitize_provider_message;

/// [`RagStorage`] over the in-process OAGW client, dispatching by storage kind.
pub struct OagwRagStorage {
    gw: Arc<dyn ServiceGatewayClientV1>,
    identity: Arc<ServiceIdentity>,
}

impl OagwRagStorage {
    #[must_use]
    pub fn new(gw: Arc<dyn ServiceGatewayClientV1>, identity: Arc<ServiceIdentity>) -> Self {
        Self { gw, identity }
    }

    /// Send `req` with the gear's S2S identity; returns the 2xx response body.
    async fn send(&self, req: http::Request<Body>) -> Result<Bytes, StorageError> {
        let ctx = self
            .identity
            .get()
            .await
            .map_err(|_| StorageError::Unavailable("service identity not ready".to_owned()))?;
        let resp = self
            .gw
            .proxy_request(ctx, req)
            .await
            .map_err(|e| gateway_error(&e))?;
        let status = resp.status();
        let gateway = from_gateway(&resp);
        let body = resp.into_body().into_bytes().await;
        if gateway {
            tracing::warn!(
                status = status.as_u16(),
                "storage request failed in the gateway"
            );
            return Err(StorageError::Transient(format!(
                "provider unavailable (gateway HTTP {})",
                status.as_u16()
            )));
        }
        if status.is_success() {
            return body.map_err(|e| {
                tracing::warn!(error = %e, "storage response body read failed");
                StorageError::Transient("provider response could not be read".to_owned())
            });
        }
        if status == http::StatusCode::NOT_FOUND {
            return Err(StorageError::NotFound);
        }
        let message = provider_message(&body.unwrap_or_default(), status);
        if status == http::StatusCode::TOO_MANY_REQUESTS || status.is_server_error() {
            Err(StorageError::Transient(message))
        } else {
            Err(StorageError::Failed(message))
        }
    }

    async fn call_json(
        &self,
        method: http::Method,
        st: &ResolvedStorage,
        tail: &str,
        body: Option<&Value>,
    ) -> Result<Value, StorageError> {
        let uri = uri(st, tail)?;
        let builder = http::Request::builder().method(method).uri(uri);
        let req = match body {
            Some(v) => builder
                .header(http::header::CONTENT_TYPE, "application/json")
                .body(Body::from(serde_json::to_vec(v).map_err(|e| {
                    StorageError::Failed(format!("request encoding failed: {e}"))
                })?)),
            None => builder.body(Body::Empty),
        }
        .map_err(|e| StorageError::Failed(format!("invalid storage request: {e}")))?;
        let bytes = self.send(req).await?;
        Ok(serde_json::from_slice(&bytes).unwrap_or(Value::Null))
    }
}

#[async_trait]
impl RagStorage for OagwRagStorage {
    async fn upload_file(
        &self,
        st: &ResolvedStorage,
        filename: &str,
        content_type: &str,
        bytes: Bytes,
    ) -> Result<String, StorageError> {
        let req = MultipartBody::new()
            .text("purpose", "assistants")
            .part(
                Part::bytes("file", bytes)
                    .filename(filename)
                    .content_type(content_type),
            )
            .into_request(http::Method::POST, uri(st, "/files")?)
            .map_err(|e| StorageError::Failed(format!("invalid storage request: {e}")))?;
        let resp = self.send(req).await?;
        id_of(&serde_json::from_slice(&resp).unwrap_or(Value::Null))
    }

    async fn delete_file(&self, st: &ResolvedStorage, file_id: &str) -> Result<(), StorageError> {
        let tail = format!("/files/{}", segment(file_id)?);
        self.call_json(http::Method::DELETE, st, &tail, None)
            .await
            .map(drop)
    }

    async fn create_vector_store(
        &self,
        st: &ResolvedStorage,
        name: &str,
    ) -> Result<String, StorageError> {
        let body = json!({ "name": name });
        let v = self
            .call_json(http::Method::POST, st, "/vector_stores", Some(&body))
            .await?;
        id_of(&v)
    }

    async fn add_file_to_vector_store(
        &self,
        st: &ResolvedStorage,
        vs: &str,
        file_id: &str,
        attachment_id: Uuid,
    ) -> Result<IndexStatus, StorageError> {
        let tail = format!("/vector_stores/{}/files", segment(vs)?);
        let body = json!({
            "file_id": file_id,
            "attributes": { "attachment_id": attachment_id.to_string() },
        });
        let v = self
            .call_json(http::Method::POST, st, &tail, Some(&body))
            .await?;
        Ok(index_status(&v))
    }

    async fn vector_store_file_status(
        &self,
        st: &ResolvedStorage,
        vs: &str,
        file_id: &str,
    ) -> Result<IndexStatus, StorageError> {
        let tail = format!(
            "/vector_stores/{}/files/{}",
            segment(vs)?,
            segment(file_id)?
        );
        let v = self.call_json(http::Method::GET, st, &tail, None).await?;
        Ok(index_status(&v))
    }

    async fn delete_vector_store(
        &self,
        st: &ResolvedStorage,
        vs: &str,
    ) -> Result<(), StorageError> {
        let tail = format!("/vector_stores/{}", segment(vs)?);
        self.call_json(http::Method::DELETE, st, &tail, None)
            .await
            .map(drop)
    }
}

/// `/{alias}{prefix}{tail}` for the storage flavour of `st`.
fn uri(st: &ResolvedStorage, tail: &str) -> Result<String, StorageError> {
    match st.kind {
        StorageKind::Openai => Ok(openai::uri(&st.alias, tail)),
        StorageKind::Azure => {
            let version = st.api_version.as_deref().ok_or_else(|| {
                StorageError::Failed(format!(
                    "azure storage '{}' has no api_version configured",
                    st.provider_id
                ))
            })?;
            Ok(azure::uri(&st.alias, version, tail))
        }
    }
}

/// A provider-issued id used as a path segment; rejects anything that could
/// change the request path.
fn segment(id: &str) -> Result<&str, StorageError> {
    let ok = !id.is_empty()
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | ':'));
    if ok {
        Ok(id)
    } else {
        Err(StorageError::Failed(
            "invalid provider object id".to_owned(),
        ))
    }
}

fn id_of(v: &Value) -> Result<String, StorageError> {
    v.get("id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| StorageError::Failed("provider response has no id".to_owned()))
}

/// Missing / null status is `InProgress`; anything but `in_progress` /
/// `completed` is `Failed` (DESIGN section 3.6).
fn index_status(v: &Value) -> IndexStatus {
    match v.get("status").and_then(Value::as_str) {
        None | Some("in_progress") => IndexStatus::InProgress,
        Some("completed") => IndexStatus::Completed,
        Some(_) => IndexStatus::Failed,
    }
}

fn gateway_error(e: &CanonicalError) -> StorageError {
    tracing::warn!(error = %e, "storage request failed in the gateway");
    StorageError::Transient("provider request failed".to_owned())
}

/// The provider's sanitized `error.message`, else `provider returned HTTP N`.
fn provider_message(body: &[u8], status: http::StatusCode) -> String {
    serde_json::from_slice::<Value>(body)
        .ok()
        .and_then(|v| {
            v.pointer("/error/message")
                .and_then(Value::as_str)
                .filter(|m| !m.trim().is_empty())
                .map(sanitize_provider_message)
        })
        .unwrap_or_else(|| format!("provider returned HTTP {}", status.as_u16()))
}

#[cfg(test)]
#[path = "storage_tests.rs"]
mod storage_tests;
