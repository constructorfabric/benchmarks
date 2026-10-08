//! File and vector-store clients over the OAGW proxy (S§9.3).
//!
//! `OpenAI` is reached under `/{alias}/v1`, Azure `OpenAI` under
//! `/{alias}/openai` with `?api-version={api_version}`. Every call carries the
//! S2S security context. Provider HTTP 5xx and gateway failures (including
//! the gateway's own 429) are [`StorageError::Transient`], anything else
//! (a provider 429 included, D "File Upload") is [`StorageError::Permanent`].

pub mod anthropic_files;
mod files;
pub mod knowledge;
mod vector_stores;

use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use oagw_sdk::api::ErrorSource;
use oagw_sdk::body::Body;
use oagw_sdk::{MultipartBody, ServiceGatewayClientV1, ServiceGatewayError};
use serde_json::Value;
use toolkit_canonical_errors::CanonicalError;
use tracing::warn;
use uuid::Uuid;

use crate::config::StorageKind;
use crate::domain::ports::{IndexStatus, StorageError, StoragePort};
use crate::infra::llm::gateway::{ERROR_BODY_LIMIT, error_message, read_limited};
use crate::infra::llm::provider_resolver::StorageTarget;
use crate::infra::llm::sanitize::sanitize_provider_message;
use crate::infra::s2s::S2sContextProvider;

/// Bytes of a successful response body that are parsed.
const RESPONSE_BODY_LIMIT: usize = 1024 * 1024;

/// Request payload.
enum Payload {
    Empty,
    Json(Value),
    Multipart(MultipartBody),
}

/// `OpenAI` / Azure `OpenAI` storage client.
pub struct RagStorage {
    oagw: Arc<dyn ServiceGatewayClientV1>,
    s2s: Arc<S2sContextProvider>,
}

impl RagStorage {
    #[must_use]
    pub fn new(oagw: Arc<dyn ServiceGatewayClientV1>, s2s: Arc<S2sContextProvider>) -> Self {
        Self { oagw, s2s }
    }

    /// Proxy URI `/{alias}{prefix}{path}` (+ `?api-version=` on Azure).
    fn uri(t: &StorageTarget, path: &str) -> Result<String, StorageError> {
        match t.storage_kind {
            StorageKind::Openai => Ok(format!("/{}/v1{path}", t.alias)),
            StorageKind::Azure => {
                let version = t.api_version.as_deref().ok_or_else(|| {
                    StorageError::Permanent(format!(
                        "provider {:?} has storage_kind azure but no api_version",
                        t.provider_id
                    ))
                })?;
                Ok(format!("/{}/openai{path}?api-version={version}", t.alias))
            }
        }
    }

    /// Send one request and return its parsed JSON body (`Null` when the body
    /// is empty or not JSON). With `ok_not_found`, a 404 from the provider is
    /// success (`Null`); a 404 produced by the gateway itself (route missing)
    /// is not.
    async fn call(
        &self,
        op: &str,
        t: &StorageTarget,
        method: http::Method,
        path: &str,
        payload: Payload,
        ok_not_found: bool,
    ) -> Result<Value, StorageError> {
        self.call_with_headers(op, t, method, path, payload, ok_not_found, &[])
            .await
    }

    /// [`Self::call`] with extra request headers.
    #[allow(clippy::too_many_arguments)]
    async fn call_with_headers(
        &self,
        op: &str,
        t: &StorageTarget,
        method: http::Method,
        path: &str,
        payload: Payload,
        ok_not_found: bool,
        headers: &[(&str, &str)],
    ) -> Result<Value, StorageError> {
        let uri = Self::uri(t, path)?;
        let ctx = self.s2s.get().await.map_err(|e| {
            warn!(error = %e, op, "S2S security context unavailable for the storage call");
            StorageError::Transient(format!("{op}: service context unavailable"))
        })?;
        let mut builder = http::Request::builder()
            .method(method)
            .uri(uri)
            .header(http::header::ACCEPT, "application/json");
        for (name, value) in headers {
            builder = builder.header(*name, *value);
        }
        let req = match payload {
            Payload::Empty => builder.body(Body::from(Vec::new())),
            Payload::Json(v) => builder
                .header(http::header::CONTENT_TYPE, "application/json")
                .body(Body::from(v.to_string().into_bytes())),
            Payload::Multipart(m) => builder
                .header(http::header::CONTENT_TYPE, m.content_type_header_value())
                .body(m.into_body()),
        }
        .map_err(|e| StorageError::Permanent(format!("{op}: invalid request: {e}")))?;

        let resp = self
            .oagw
            .proxy_request(ctx, req)
            .await
            .map_err(|e| proxy_error(op, e))?;
        let status = resp.status();
        let from_gateway = resp.extensions().get::<ErrorSource>() == Some(&ErrorSource::Gateway);
        if status == http::StatusCode::NOT_FOUND && ok_not_found && !from_gateway {
            return Ok(Value::Null);
        }
        if status.is_success() {
            let body = read_limited(resp.into_body(), RESPONSE_BODY_LIMIT)
                .await
                .map_err(|()| StorageError::Transient(format!("{op}: response read failed")))?;
            return Ok(serde_json::from_slice(&body).unwrap_or(Value::Null));
        }
        Err(http_error(op, status, from_gateway, resp.into_body()).await)
    }
}

/// The proxy failed before any response: timeouts, unavailability and
/// throttling are transient, everything else (auth, validation, missing
/// route, ...) is not.
fn proxy_error(op: &str, err: CanonicalError) -> StorageError {
    warn!(op, error = %err.detail(), "OAGW proxy call failed");
    match ServiceGatewayError::from(err) {
        ServiceGatewayError::AuthFailed { .. }
        | ServiceGatewayError::PermissionDenied { .. }
        | ServiceGatewayError::PayloadTooLarge { .. }
        | ServiceGatewayError::InvalidTargetHost { .. }
        | ServiceGatewayError::Validation { .. }
        | ServiceGatewayError::NotFound { .. }
        | ServiceGatewayError::AlreadyExists { .. }
        | ServiceGatewayError::FailedPrecondition { .. } => {
            StorageError::Permanent(format!("{op}: gateway rejected the request"))
        }
        _ => StorageError::Transient(format!("{op}: gateway failure")),
    }
}

/// Non-2xx response → error class by status, message from the sanitized
/// provider error text (never a gateway body).
async fn http_error(
    op: &str,
    status: http::StatusCode,
    from_gateway: bool,
    body: Body,
) -> StorageError {
    let body = read_limited(body, ERROR_BODY_LIMIT)
        .await
        .unwrap_or_default();
    warn!(
        op,
        status = status.as_u16(),
        from_gateway,
        body = %String::from_utf8_lossy(&body[..body.len().min(2048)]),
        "provider storage call failed"
    );
    let provider_message = if from_gateway {
        None
    } else {
        serde_json::from_slice::<Value>(&body)
            .ok()
            .as_ref()
            .and_then(error_message)
    };
    let mut message = format!("{op} failed: HTTP {}", status.as_u16());
    if let Some(m) = provider_message {
        message = format!("{message}: {}", sanitize_provider_message(&m));
    }
    // D "File Upload": transient = provider 5xx or gateway failure (the
    // gateway's own throttling included); a provider 429 is permanent.
    let gateway_throttled = from_gateway && status == http::StatusCode::TOO_MANY_REQUESTS;
    if status.is_server_error() || gateway_throttled {
        StorageError::Transient(message)
    } else {
        StorageError::Permanent(message)
    }
}

/// A provider id interpolated into a path: letters, digits, `_`, `-`, `.`.
fn path_id<'a>(kind: &str, id: &'a str) -> Result<&'a str, StorageError> {
    let valid = !id.is_empty()
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'));
    if valid {
        Ok(id)
    } else {
        Err(StorageError::Permanent(format!("invalid {kind} id")))
    }
}

#[async_trait]
impl StoragePort for RagStorage {
    async fn upload_file(
        &self,
        t: &StorageTarget,
        filename: &str,
        content_type: &str,
        bytes: Bytes,
    ) -> Result<String, StorageError> {
        self.do_upload_file(t, filename, content_type, bytes).await
    }

    async fn delete_file(&self, t: &StorageTarget, file_id: &str) -> Result<(), StorageError> {
        self.do_delete_file(t, file_id).await
    }

    async fn create_vector_store(
        &self,
        t: &StorageTarget,
        chat_id: Uuid,
    ) -> Result<String, StorageError> {
        self.do_create_vector_store(t, chat_id).await
    }

    async fn add_file_to_vector_store(
        &self,
        t: &StorageTarget,
        vs: &str,
        file_id: &str,
        attachment_id: Uuid,
    ) -> Result<IndexStatus, StorageError> {
        self.do_add_file(t, vs, file_id, attachment_id).await
    }

    async fn vector_store_file_status(
        &self,
        t: &StorageTarget,
        vs: &str,
        file_id: &str,
    ) -> Result<IndexStatus, StorageError> {
        self.do_file_status(t, vs, file_id).await
    }

    async fn delete_vector_store(&self, t: &StorageTarget, vs: &str) -> Result<(), StorageError> {
        self.do_delete_vector_store(t, vs).await
    }
}

#[cfg(test)]
#[path = "storage_tests.rs"]
mod tests;
