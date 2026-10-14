//! The OAGW adapter of the storage-provider port: the Files API, the Vector Stores API and the
//! Anthropic Files client of the upload path.
//!
//! Every call goes through the in-process gateway client under the gear's S2S security context
//! (implementation note 5), on the URI `/{upstream_alias}{prefix}{path}` where the prefix is `/v1`
//! for `storage_kind` `openai` and `/openai` plus the `api-version` query for `azure`; OAGW
//! injects the credentials, so no credential or idempotency header is ever set here. Provider
//! writes are not retried. Provider text never leaves this adapter: the callers get a
//! [`ProviderFailure`] / [`UploadError`] classification only.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use futures::StreamExt;
use oagw_sdk::Body;
use oagw_sdk::api::ServiceGatewayClientV1;
use serde_json::Value;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::config::StorageKind;
use crate::domain::attachments::provider::{
    FileUploadMeta, ProviderFailure, StorageProvider, StorageTarget, UploadBody, UploadBodyError,
    UploadError,
};
use crate::infra::provider_task::S2sContext;

/// The most bytes of a provider answer that are read (the answers of these routes are small JSON
/// objects).
const MAX_ANSWER_BYTES: usize = 1 << 20;

/// The adapter over the gateway client.
pub struct OagwStorageClient {
    gateway: Arc<dyn ServiceGatewayClientV1>,
    s2s: Arc<S2sContext>,
}

/// A provider answer: the status and the JSON body (`Null` when it is not JSON).
struct Answer {
    status: u16,
    json: Value,
}

impl Answer {
    fn success(&self) -> bool {
        (200..300).contains(&self.status)
    }

    fn id(&self) -> Option<String> {
        if self.success() {
            self.json["id"].as_str().map(str::to_owned)
        } else {
            None
        }
    }

    fn indexing_status(&self) -> Option<String> {
        self.json["status"].as_str().map(str::to_owned)
    }
}

/// The gateway failed (or the S2S context is not bound, or the body stream aborted): no provider
/// answer exists.
#[derive(Debug, Clone, Copy)]
struct GatewayFailure;

/// One part of a multipart form: a field name, the file name and content type for a file part.
struct Part<'a> {
    name: &'a str,
    file: Option<(&'a str, &'a str)>,
    data: &'a [u8],
}

/// Make a value safe inside a quoted multipart header parameter: no quote, backslash or line
/// break may survive.
fn header_safe(value: &str) -> String {
    value
        .chars()
        .filter(|c| !matches!(c, '"' | '\\' | '\r' | '\n'))
        .collect()
}

fn part_header(boundary: &str, name: &str, file: Option<(&str, &str)>) -> Vec<u8> {
    let mut out = format!("--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"");
    if let Some((filename, content_type)) = file {
        out = format!(
            "{out}; filename=\"{}\"\r\nContent-Type: {}",
            header_safe(filename),
            header_safe(content_type)
        );
    }
    out.push_str("\r\n\r\n");
    out.into_bytes()
}

fn new_boundary() -> String {
    format!("mc-{}", Uuid::new_v4().simple())
}

fn multipart_type(boundary: &str) -> String {
    format!("multipart/form-data; boundary={boundary}")
}

/// A small, fully buffered multipart body.
fn multipart(parts: &[Part<'_>]) -> (String, Bytes) {
    let boundary = new_boundary();
    let mut out = Vec::new();
    for part in parts {
        out.extend_from_slice(&part_header(&boundary, part.name, part.file));
        out.extend_from_slice(part.data);
        out.extend_from_slice(b"\r\n");
    }
    out.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
    (multipart_type(&boundary), Bytes::from(out))
}

/// Read a response body, at most [`MAX_ANSWER_BYTES`].
async fn read_bounded(body: Body) -> Result<Vec<u8>, GatewayFailure> {
    let mut stream = body.into_stream();
    let mut out = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| GatewayFailure)?;
        if out.len() + chunk.len() > MAX_ANSWER_BYTES {
            return Err(GatewayFailure);
        }
        out.extend_from_slice(&chunk);
    }
    Ok(out)
}

impl OagwStorageClient {
    /// Create the adapter.
    #[must_use]
    pub fn new(gateway: Arc<dyn ServiceGatewayClientV1>, s2s: Arc<S2sContext>) -> Self {
        Self { gateway, s2s }
    }

    /// `/{alias}{prefix}{path}{query}`.
    fn uri(target: &StorageTarget, path: &str) -> String {
        let (prefix, query) = match target.storage_kind {
            StorageKind::Openai => ("/v1", String::new()),
            StorageKind::Azure => (
                "/openai",
                target
                    .api_version
                    .as_ref()
                    .map(|version| format!("?api-version={version}"))
                    .unwrap_or_default(),
            ),
        };
        format!("/{}{prefix}{path}{query}", target.upstream_alias)
    }

    async fn call(
        &self,
        method: http::Method,
        uri: String,
        content_type: Option<String>,
        body: Body,
    ) -> Result<Answer, GatewayFailure> {
        let ctx: SecurityContext = self.s2s.get().cloned().ok_or(GatewayFailure)?;
        let mut builder = http::Request::builder().method(method).uri(uri);
        if let Some(content_type) = content_type {
            builder = builder.header(http::header::CONTENT_TYPE, content_type);
        }
        let request = builder.body(body).map_err(|_| GatewayFailure)?;
        let response = self
            .gateway
            .proxy_request(ctx, request)
            .await
            .map_err(|_| GatewayFailure)?;
        let status = response.status().as_u16();
        let bytes = read_bounded(response.into_body()).await?;
        Ok(Answer {
            status,
            json: serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        })
    }

    async fn call_json(
        &self,
        method: http::Method,
        uri: String,
        body: Option<Value>,
    ) -> Result<Answer, GatewayFailure> {
        match body {
            Some(value) => {
                self.call(
                    method,
                    uri,
                    Some("application/json".to_owned()),
                    Body::from(Bytes::from(value.to_string())),
                )
                .await
            }
            None => self.call(method, uri, None, Body::Empty).await,
        }
    }

    /// A provider 5xx and a gateway failure are transient, anything else is not.
    fn failure(answer: &Result<Answer, GatewayFailure>) -> ProviderFailure {
        match answer {
            Ok(answer) if answer.status < 500 => ProviderFailure::Other,
            Ok(_) | Err(GatewayFailure) => ProviderFailure::Transient,
        }
    }

    async fn delete(&self, target: &StorageTarget, path: &str) -> Result<(), ProviderFailure> {
        let answer = self
            .call_json(http::Method::DELETE, Self::uri(target, path), None)
            .await;
        match &answer {
            Ok(answer) if answer.success() => Ok(()),
            Ok(answer) if answer.status == 404 => Err(ProviderFailure::NotFound),
            _ => Err(Self::failure(&answer)),
        }
    }
}

#[async_trait]
impl StorageProvider for OagwStorageClient {
    async fn upload_file(
        &self,
        target: &StorageTarget,
        meta: &FileUploadMeta,
        body: UploadBody,
    ) -> Result<String, UploadError> {
        // the multipart body is streamed: the head (the `purpose` field and the opening of the
        // `file` part), every chunk as it arrives, then the closing boundary; a failing source
        // stream ends the request body with an error, which drops the provider request
        let boundary = new_boundary();
        let mut head = part_header(&boundary, "purpose", None);
        head.extend_from_slice(b"assistants\r\n");
        head.extend_from_slice(&part_header(
            &boundary,
            "file",
            Some((&meta.provider_filename, &meta.content_type)),
        ));
        let tail = Bytes::from(format!("\r\n--{boundary}--\r\n"));
        let aborted: Arc<Mutex<Option<UploadBodyError>>> = Arc::new(Mutex::new(None));
        let record = Arc::clone(&aborted);
        let chunks = body.map(move |item| {
            item.map_err(|error| {
                if let Ok(mut slot) = record.lock() {
                    *slot = Some(error);
                }
                oagw_sdk::body::BoxError::from("the upload body stream failed")
            })
        });
        let stream = futures::stream::iter([Ok(Bytes::from(head))])
            .chain(chunks)
            .chain(futures::stream::iter([Ok(tail)]));
        let answer = self
            .call(
                http::Method::POST,
                Self::uri(target, "/files"),
                Some(multipart_type(&boundary)),
                Body::Stream(Box::pin(stream)),
            )
            .await;
        if let Some(error) = aborted.lock().ok().and_then(|slot| *slot) {
            return Err(UploadError::Body(error));
        }
        answer
            .ok()
            .and_then(|answer| answer.id())
            .ok_or(UploadError::Provider)
    }

    async fn delete_file(
        &self,
        target: &StorageTarget,
        provider_file_id: &str,
    ) -> Result<(), ProviderFailure> {
        self.delete(target, &format!("/files/{provider_file_id}"))
            .await
    }

    async fn create_vector_store(&self, target: &StorageTarget) -> Result<String, ProviderFailure> {
        let answer = self
            .call_json(
                http::Method::POST,
                Self::uri(target, "/vector_stores"),
                Some(serde_json::json!({})),
            )
            .await;
        match &answer {
            // a 2xx answer without an id is not a store
            Ok(answer) if answer.success() => answer.id().ok_or(ProviderFailure::Other),
            _ => Err(Self::failure(&answer)),
        }
    }

    async fn delete_vector_store(
        &self,
        target: &StorageTarget,
        vector_store_id: &str,
    ) -> Result<(), ProviderFailure> {
        self.delete(target, &format!("/vector_stores/{vector_store_id}"))
            .await
    }

    async fn add_vector_store_file(
        &self,
        target: &StorageTarget,
        vector_store_id: &str,
        provider_file_id: &str,
        attachment_id: Uuid,
    ) -> Result<Option<String>, ProviderFailure> {
        // exactly one attribute: the attachment id the citation mapping resolves
        let body = serde_json::json!({
            "file_id": provider_file_id,
            "attributes": { "attachment_id": attachment_id.to_string() }
        });
        let answer = self
            .call_json(
                http::Method::POST,
                Self::uri(target, &format!("/vector_stores/{vector_store_id}/files")),
                Some(body),
            )
            .await;
        match &answer {
            Ok(answer) if answer.success() => Ok(answer.indexing_status()),
            _ => Err(Self::failure(&answer)),
        }
    }

    async fn read_vector_store_file(
        &self,
        target: &StorageTarget,
        vector_store_id: &str,
        provider_file_id: &str,
        timeout: Duration,
    ) -> Result<Option<String>, ProviderFailure> {
        let call = self.call_json(
            http::Method::GET,
            Self::uri(
                target,
                &format!("/vector_stores/{vector_store_id}/files/{provider_file_id}"),
            ),
            None,
        );
        // the request deadline of this one call; the pacing between calls is the caller's timer
        let Ok(answer) = tokio::time::timeout(timeout, call).await else {
            return Err(ProviderFailure::Transient);
        };
        match &answer {
            Ok(answer) if answer.success() => Ok(answer.indexing_status()),
            _ => Err(Self::failure(&answer)),
        }
    }

    async fn upload_secondary_image(
        &self,
        target: &StorageTarget,
        filename: &str,
        content_type: &str,
        bytes: Bytes,
    ) -> Result<String, ProviderFailure> {
        // only the `file` part, no `purpose`
        let (header, payload) = multipart(&[Part {
            name: "file",
            file: Some((filename, content_type)),
            data: &bytes,
        }]);
        let answer = self
            .call(
                http::Method::POST,
                Self::uri(target, "/files"),
                Some(header),
                Body::from(payload),
            )
            .await;
        match &answer {
            Ok(answer) if answer.success() => answer.id().ok_or(ProviderFailure::Other),
            _ => Err(Self::failure(&answer)),
        }
    }

    async fn delete_secondary_file(
        &self,
        target: &StorageTarget,
        file_id: &str,
    ) -> Result<(), ProviderFailure> {
        self.delete(target, &format!("/files/{file_id}")).await
    }
}

#[cfg(test)]
#[path = "storage_client_tests.rs"]
mod storage_client_tests;
