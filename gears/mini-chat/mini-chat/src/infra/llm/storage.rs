//! Files API and Vector Stores API through OAGW (`OpenAI` or Azure layout).

use std::sync::Arc;

use bytes::Bytes;
use oagw_sdk::{Body, ServiceGatewayClientV1};
use serde_json::{Value, json};
use toolkit_security::SecurityContext;

use super::{S2sContext, StorageTarget};

/// Storage failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StorageError {
    pub message: String,
    /// Transient (5xx, gateway failure) vs permanent.
    pub transient: bool,
    pub status: Option<u16>,
}

impl StorageError {
    fn new(message: impl Into<String>, transient: bool, status: Option<u16>) -> Self {
        Self {
            message: message.into(),
            transient,
            status,
        }
    }
}

impl std::fmt::Display for StorageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

/// Indexing status of a vector store file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexStatus {
    InProgress,
    Completed,
    Failed,
}

fn index_status(v: &Value) -> IndexStatus {
    match v.get("status").and_then(Value::as_str) {
        None | Some("in_progress") => IndexStatus::InProgress,
        Some("completed") => IndexStatus::Completed,
        Some(_) => IndexStatus::Failed,
    }
}

/// Storage client.
pub struct StorageClient {
    gateway: Arc<dyn ServiceGatewayClientV1>,
    s2s: Arc<S2sContext>,
    resolver: Option<Arc<super::ProviderResolver>>,
}

impl StorageClient {
    #[must_use]
    pub fn new(gateway: Arc<dyn ServiceGatewayClientV1>, s2s: Arc<S2sContext>) -> Self {
        Self {
            gateway,
            s2s,
            resolver: None,
        }
    }

    /// Enables on-demand provisioning of pending upstreams.
    #[must_use]
    pub fn with_resolver(mut self, resolver: Arc<super::ProviderResolver>) -> Self {
        self.resolver = Some(resolver);
        self
    }

    async fn call(
        &self,
        ctx: &SecurityContext,
        method: http::Method,
        uri: String,
        content_type: Option<String>,
        body: Body,
    ) -> Result<(u16, Bytes), StorageError> {
        let mut b = http::Request::builder().method(method).uri(uri);
        if let Some(ct) = content_type {
            b = b.header(http::header::CONTENT_TYPE, ct);
        }
        let req = b
            .body(body)
            .map_err(|e| StorageError::new(e.to_string(), false, None))?;
        if let Some(r) = &self.resolver {
            let alias = req.uri().path().trim_start_matches('/').split('/').next().unwrap_or_default().to_owned();
            r.ensure_ready(&alias).await;
        }
        let resp = self
            .gateway
            .proxy_request(self.s2s.proxy_ctx(ctx), req)
            .await
            .map_err(|e| {
                tracing::warn!(error = %e, "storage request failed at the gateway");
                StorageError::new(format!("gateway error: {e}"), true, None)
            })?;
        let status = resp.status().as_u16();
        let bytes = resp.into_body().into_bytes().await.unwrap_or_default();
        Ok((status, bytes))
    }

    fn uri(target: &StorageTarget, path: &str) -> String {
        format!("/{}{}{}{}", target.alias, target.prefix(), path, target.query())
    }

    fn check(status: u16, body: &Bytes, what: &str) -> Result<Value, StorageError> {
        if (200..300).contains(&status) {
            return Ok(serde_json::from_slice(body).unwrap_or(Value::Null));
        }
        Err(StorageError::new(
            format!("{what} failed with HTTP {status}"),
            status >= 500 || status == 429,
            Some(status),
        ))
    }

    /// Uploads a file (`purpose=assistants`); returns the provider file id.
    ///
    /// # Errors
    /// Provider or gateway failure.
    pub async fn upload_file(
        &self,
        ctx: &SecurityContext,
        target: &StorageTarget,
        filename: &str,
        content_type: &str,
        data: Bytes,
    ) -> Result<String, StorageError> {
        let boundary = format!("----minichat{}", uuid::Uuid::new_v4().simple());
        let mut body = Vec::with_capacity(data.len() + 512);
        body.extend_from_slice(
            format!("--{boundary}\r\nContent-Disposition: form-data; name=\"purpose\"\r\n\r\nassistants\r\n")
                .as_bytes(),
        );
        let safe_name = filename.replace(['"', '\r', '\n'], "_");
        body.extend_from_slice(
            format!(
                "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{safe_name}\"\r\nContent-Type: {content_type}\r\n\r\n"
            )
            .as_bytes(),
        );
        body.extend_from_slice(&data);
        body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
        let (status, bytes) = self
            .call(
                ctx,
                http::Method::POST,
                Self::uri(target, "/files"),
                Some(format!("multipart/form-data; boundary={boundary}")),
                Body::from(body),
            )
            .await?;
        let v = Self::check(status, &bytes, "file upload")?;
        v.get("id")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned)
            .ok_or_else(|| StorageError::new("file upload returned no id", false, Some(status)))
    }

    /// Deletes a file; 404 counts as success.
    ///
    /// # Errors
    /// Provider or gateway failure.
    pub async fn delete_file(
        &self,
        ctx: &SecurityContext,
        target: &StorageTarget,
        file_id: &str,
    ) -> Result<(), StorageError> {
        let (status, bytes) = self
            .call(
                ctx,
                http::Method::DELETE,
                Self::uri(target, &format!("/files/{file_id}")),
                None,
                Body::Empty,
            )
            .await?;
        if status == 404 {
            return Ok(());
        }
        Self::check(status, &bytes, "file delete").map(|_| ())
    }

    /// Creates a vector store; returns its id.
    ///
    /// # Errors
    /// Provider or gateway failure.
    pub async fn create_vector_store(
        &self,
        ctx: &SecurityContext,
        target: &StorageTarget,
        name: &str,
    ) -> Result<String, StorageError> {
        let body = serde_json::to_vec(&json!({"name": name})).unwrap_or_default();
        let (status, bytes) = self
            .call(
                ctx,
                http::Method::POST,
                Self::uri(target, "/vector_stores"),
                Some("application/json".to_owned()),
                Body::from(body),
            )
            .await?;
        let v = Self::check(status, &bytes, "vector store create")?;
        v.get("id")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned)
            .ok_or_else(|| StorageError::new("vector store create returned no id", false, Some(status)))
    }

    /// Adds a file to a vector store with the `attachment_id` attribute.
    ///
    /// # Errors
    /// Provider or gateway failure.
    pub async fn add_file_to_vector_store(
        &self,
        ctx: &SecurityContext,
        target: &StorageTarget,
        vector_store_id: &str,
        file_id: &str,
        attachment_id: uuid::Uuid,
    ) -> Result<IndexStatus, StorageError> {
        let body = serde_json::to_vec(
            &json!({"file_id": file_id, "attributes": {"attachment_id": attachment_id.to_string()}}),
        )
        .unwrap_or_default();
        let (status, bytes) = self
            .call(
                ctx,
                http::Method::POST,
                Self::uri(target, &format!("/vector_stores/{vector_store_id}/files")),
                Some("application/json".to_owned()),
                Body::from(body),
            )
            .await?;
        let v = Self::check(status, &bytes, "vector store file add")?;
        Ok(index_status(&v))
    }

    /// Reads the indexing status of a vector store file.
    ///
    /// # Errors
    /// Provider or gateway failure.
    pub async fn get_vector_store_file(
        &self,
        ctx: &SecurityContext,
        target: &StorageTarget,
        vector_store_id: &str,
        file_id: &str,
    ) -> Result<IndexStatus, StorageError> {
        let (status, bytes) = self
            .call(
                ctx,
                http::Method::GET,
                Self::uri(target, &format!("/vector_stores/{vector_store_id}/files/{file_id}")),
                None,
                Body::Empty,
            )
            .await?;
        let v = Self::check(status, &bytes, "vector store file status")?;
        Ok(index_status(&v))
    }

    /// Deletes a vector store; 404 counts as success.
    ///
    /// # Errors
    /// Provider or gateway failure.
    pub async fn delete_vector_store(
        &self,
        ctx: &SecurityContext,
        target: &StorageTarget,
        vector_store_id: &str,
    ) -> Result<(), StorageError> {
        let (status, bytes) = self
            .call(
                ctx,
                http::Method::DELETE,
                Self::uri(target, &format!("/vector_stores/{vector_store_id}")),
                None,
                Body::Empty,
            )
            .await?;
        if status == 404 {
            return Ok(());
        }
        Self::check(status, &bytes, "vector store delete").map(|_| ())
    }
}
