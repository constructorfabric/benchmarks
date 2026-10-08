//! OpenAI-compatible file and vector store client (`OpenAI` and Azure `OpenAI`).

use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use http::{Method, StatusCode};
use oagw_sdk::api::ErrorSource;
use oagw_sdk::{Body, MultipartBody, Part, ServiceGatewayClientV1};
use serde_json::{Value, json};
use uuid::Uuid;

use super::{FileStorage, FileUpload, IndexStatus, StorageError, VectorStores};
use crate::domain::sanitize::sanitize_provider_message;
use crate::infra::llm::errors::{error_fields, from_canonical};
use crate::infra::llm::{S2sContext, StorageTarget};

/// Storage client over the in-process OAGW client; every call carries the S2S context.
pub struct OpenAiStorage {
    gateway: Arc<dyn ServiceGatewayClientV1>,
    s2s: S2sContext,
}

/// A response that reached us (as opposed to a failure of `proxy_request` itself).
pub(super) struct Reply {
    source: Option<ErrorSource>,
    pub(super) status: StatusCode,
    body: Bytes,
}

impl Reply {
    /// Gateway-generated responses (route missing, 502/503/504) are never "the provider said so".
    pub(super) fn is_gateway_error(&self) -> bool {
        self.source == Some(ErrorSource::Gateway)
    }

    /// The body of a 2xx response; anything else becomes a [`StorageError`].
    pub(super) fn into_success(self) -> Result<Bytes, StorageError> {
        if self.status.is_success() && !self.is_gateway_error() {
            Ok(self.body)
        } else {
            Err(self.into_error())
        }
    }

    fn into_error(self) -> StorageError {
        let status = self.status.as_u16();
        if self.is_gateway_error() {
            return StorageError::Transient(if self.status == StatusCode::GATEWAY_TIMEOUT {
                "storage request timed out".to_owned()
            } else {
                format!("storage gateway error ({status})")
            });
        }
        let (_, message) = error_fields(&self.body);
        let message = message.unwrap_or_else(|| format!("storage provider returned HTTP {status}"));
        if self.status.is_client_error() {
            StorageError::Permanent { status, message }
        } else {
            StorageError::Transient(message)
        }
    }
}

impl OpenAiStorage {
    #[must_use]
    pub fn new(gateway: Arc<dyn ServiceGatewayClientV1>, s2s: S2sContext) -> Self {
        Self { gateway, s2s }
    }

    async fn send(&self, req: http::Request<Body>) -> Result<Reply, StorageError> {
        proxy(&self.gateway, &self.s2s, req).await
    }

    async fn json_call(
        &self,
        method: Method,
        uri: String,
        body: Option<Value>,
    ) -> Result<Reply, StorageError> {
        let mut builder = http::Request::builder()
            .method(method)
            .uri(uri)
            .header(http::header::ACCEPT, "application/json");
        let body = match body {
            Some(value) => {
                builder = builder.header(http::header::CONTENT_TYPE, "application/json");
                Body::from(
                    serde_json::to_vec(&value)
                        .map_err(|_| invalid_request("failed to encode the storage request"))?,
                )
            }
            None => Body::Empty,
        };
        let req = builder
            .body(body)
            .map_err(|_| invalid_request("failed to build the storage request"))?;
        self.send(req).await
    }

    /// DELETE where a missing resource counts as deleted.
    async fn delete_path(&self, t: &StorageTarget, path: &str) -> Result<(), StorageError> {
        let reply = self.json_call(Method::DELETE, t.uri(path), None).await?;
        if reply.status == StatusCode::NOT_FOUND && !reply.is_gateway_error() {
            return Ok(());
        }
        reply.into_success().map(drop)
    }

    fn status_from(reply: Reply) -> Result<IndexStatus, StorageError> {
        Ok(parse_status(&json_body(&reply.into_success()?)))
    }
}

/// Sends `req` through OAGW with the S2S context and reads the whole response.
pub(super) async fn proxy(
    gateway: &Arc<dyn ServiceGatewayClientV1>,
    s2s: &S2sContext,
    req: http::Request<Body>,
) -> Result<Reply, StorageError> {
    let ctx = s2s
        .get()
        .map_err(|e| StorageError::Transient(sanitize_provider_message(&e.to_string())))?;
    let resp = gateway
        .proxy_request(ctx, req)
        .await
        .map_err(|e| StorageError::Transient(from_canonical(&e).message))?;
    let source = resp.extensions().get::<ErrorSource>().copied();
    let (parts, body) = resp.into_parts();
    let body = body
        .into_bytes()
        .await
        .map_err(|_| StorageError::Transient("failed to read the storage response".to_owned()))?;
    Ok(Reply {
        source,
        status: parts.status,
        body,
    })
}

/// A request the client could not even build; retrying cannot help.
pub(super) fn invalid_request(message: &str) -> StorageError {
    StorageError::Permanent {
        status: 400,
        message: message.to_owned(),
    }
}

/// Provider ids go into URL paths: only the characters providers use are allowed.
pub(super) fn path_id(id: &str) -> Result<&str, StorageError> {
    let valid = !id.is_empty()
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        && id != "."
        && id != "..";
    if valid {
        Ok(id)
    } else {
        Err(invalid_request("invalid storage identifier"))
    }
}

pub(super) fn json_body(bytes: &[u8]) -> Value {
    serde_json::from_slice(bytes).unwrap_or(Value::Null)
}

/// The provider id of a created resource.
pub(super) fn created_id(body: &Bytes) -> Result<String, StorageError> {
    json_body(body)
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| StorageError::Transient("storage response carries no id".to_owned()))
}

/// Missing / `null` status is still in progress; only `completed` succeeds, every other value
/// (`failed`, `cancelled`, unknown) is a failure.
fn parse_status(body: &Value) -> IndexStatus {
    let status = match body.get("status") {
        None | Some(Value::Null) => return IndexStatus::InProgress,
        Some(Value::String(s)) => s.as_str(),
        Some(_) => "unknown",
    };
    match status {
        "in_progress" => IndexStatus::InProgress,
        "completed" => IndexStatus::Completed,
        other => {
            let detail = body
                .pointer("/last_error/message")
                .and_then(Value::as_str)
                .filter(|m| !m.is_empty());
            let reason = match detail {
                Some(message) => format!("indexing {other}: {message}"),
                None => format!("indexing {other}"),
            };
            IndexStatus::Failed(sanitize_provider_message(&reason))
        }
    }
}

#[async_trait]
impl FileStorage for OpenAiStorage {
    async fn upload(&self, t: &StorageTarget, f: FileUpload) -> Result<String, StorageError> {
        let req = MultipartBody::new()
            .text("purpose", "assistants")
            .part(
                Part::stream("file", f.body)
                    .filename(f.filename)
                    .content_type(f.content_type),
            )
            .into_request(Method::POST, t.uri("/files"))
            .map_err(|_| invalid_request("failed to build the upload request"))?;
        let body = self.send(req).await?.into_success()?;
        created_id(&body)
    }

    async fn delete(&self, t: &StorageTarget, file_id: &str) -> Result<(), StorageError> {
        self.delete_path(t, &format!("/files/{}", path_id(file_id)?))
            .await
    }
}

#[async_trait]
impl VectorStores for OpenAiStorage {
    async fn create(&self, t: &StorageTarget, name: &str) -> Result<String, StorageError> {
        let reply = self
            .json_call(
                Method::POST,
                t.uri("/vector_stores"),
                Some(json!({ "name": name })),
            )
            .await?;
        created_id(&reply.into_success()?)
    }

    async fn add_file(
        &self,
        t: &StorageTarget,
        vs_id: &str,
        file_id: &str,
        attachment_id: Uuid,
    ) -> Result<IndexStatus, StorageError> {
        let uri = t.uri(&format!("/vector_stores/{}/files", path_id(vs_id)?));
        let body = json!({
            "file_id": file_id,
            "attributes": { "attachment_id": attachment_id.to_string() },
        });
        let reply = self.json_call(Method::POST, uri, Some(body)).await?;
        Self::status_from(reply)
    }

    async fn file_status(
        &self,
        t: &StorageTarget,
        vs_id: &str,
        file_id: &str,
    ) -> Result<IndexStatus, StorageError> {
        let uri = t.uri(&format!(
            "/vector_stores/{}/files/{}",
            path_id(vs_id)?,
            path_id(file_id)?
        ));
        let reply = self.json_call(Method::GET, uri, None).await?;
        Self::status_from(reply)
    }

    async fn delete(&self, t: &StorageTarget, vs_id: &str) -> Result<(), StorageError> {
        self.delete_path(t, &format!("/vector_stores/{}", path_id(vs_id)?))
            .await
    }
}
