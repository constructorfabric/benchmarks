//! Files API / Vector Stores API over OAGW (`storage_kind` openai / azure).

use std::collections::BTreeMap;

use async_trait::async_trait;
use http::Method;
use oagw_sdk::Body;
use serde_json::{Value, json};
use uuid::Uuid;

use super::super::sanitize::sanitize_provider_message;
use super::super::{FileStorage, StorageError, VectorFileStatus};
use super::OagwProviderClient;
use super::errors::{read_body, storage_error_message};

/// Builds a `multipart/form-data` body with `purpose=assistants` and the `file` part.
pub(crate) fn multipart_body(
    boundary: &str,
    filename: &str,
    content_type: &str,
    data: &[u8],
) -> Vec<u8> {
    let safe_name: String = filename
        .chars()
        .map(|c| match c {
            '"' => "%22".to_owned(),
            '\r' => "%0D".to_owned(),
            '\n' => "%0A".to_owned(),
            c => c.to_string(),
        })
        .collect();
    let safe_ct: String = content_type
        .chars()
        .filter(|c| !matches!(c, '\r' | '\n'))
        .collect();
    let safe_ct = if safe_ct.trim().is_empty() {
        "application/octet-stream".to_owned()
    } else {
        safe_ct
    };
    let mut out = Vec::with_capacity(data.len() + 512);
    out.extend_from_slice(
        format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"purpose\"\r\n\r\nassistants\r\n"
        )
        .as_bytes(),
    );
    out.extend_from_slice(
        format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{safe_name}\"\r\nContent-Type: {safe_ct}\r\n\r\n"
        )
        .as_bytes(),
    );
    out.extend_from_slice(data);
    out.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    out
}

/// Maps a vector-store file `status` value.
pub(crate) fn parse_vector_file_status(v: &Value) -> VectorFileStatus {
    match v.get("status").and_then(Value::as_str) {
        None | Some("in_progress") => VectorFileStatus::InProgress,
        Some("completed") => VectorFileStatus::Completed,
        Some(other) => VectorFileStatus::Failed(other.to_owned()),
    }
}

fn id_of(v: &Value) -> Result<String, StorageError> {
    v.get("id")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| StorageError::Http {
            status: 502,
            message: "provider response has no id".to_owned(),
        })
}

impl OagwProviderClient {
    /// Sends a RAG request to `/{alias}{prefix}{suffix}{query}` and returns the parsed JSON
    /// body (`Null` when empty / not JSON).
    async fn storage_call(
        &self,
        provider_id: &str,
        tenant_id: Uuid,
        method: Method,
        suffix: &str,
        content_type: Option<&str>,
        body: Body,
    ) -> Result<Value, StorageError> {
        let rp = self
            .resolver
            .resolve(provider_id, tenant_id)
            .ok_or_else(|| {
                StorageError::Config(format!("unknown storage provider '{provider_id}'"))
            })?;
        let ctx = self.s2s.get().await.map_err(StorageError::Transport)?;
        let uri = format!(
            "/{}{}{}{}",
            rp.alias,
            rp.rag_prefix(),
            suffix,
            rp.rag_query()
        );
        let mut builder = http::Request::builder().method(method.clone()).uri(&uri);
        if let Some(ct) = content_type {
            builder = builder.header(http::header::CONTENT_TYPE, ct);
        }
        let req = builder
            .body(body)
            .map_err(|e| StorageError::Config(format!("invalid storage request: {e}")))?;
        let resp = self.oagw.proxy_request(ctx, req).await.map_err(|e| {
            tracing::warn!(error = %e.detail(), %method, "OAGW storage call failed");
            StorageError::Transport(sanitize_provider_message(e.detail()))
        })?;
        let status = resp.status();
        let bytes = read_body(resp.into_body()).await;
        if !status.is_success() {
            let message = storage_error_message(status, &bytes);
            tracing::debug!(status = status.as_u16(), %method, %message, "provider storage call failed");
            return Err(StorageError::Http {
                status: status.as_u16(),
                message,
            });
        }
        Ok(serde_json::from_slice(&bytes).unwrap_or(Value::Null))
    }

    async fn storage_json(
        &self,
        provider_id: &str,
        tenant_id: Uuid,
        method: Method,
        suffix: &str,
        body: &Value,
    ) -> Result<Value, StorageError> {
        let bytes = serde_json::to_vec(body)
            .map_err(|e| StorageError::Config(format!("serialize storage request: {e}")))?;
        self.storage_call(
            provider_id,
            tenant_id,
            method,
            suffix,
            Some("application/json"),
            Body::from(bytes),
        )
        .await
    }
}

#[async_trait]
impl FileStorage for OagwProviderClient {
    async fn upload_file(
        &self,
        provider_id: &str,
        tenant_id: Uuid,
        filename: &str,
        content_type: &str,
        data: bytes::Bytes,
    ) -> Result<String, StorageError> {
        let boundary = format!("----minichat{}", Uuid::new_v4().simple());
        let body = multipart_body(&boundary, filename, content_type, &data);
        let ct = format!("multipart/form-data; boundary={boundary}");
        let v = self
            .storage_call(
                provider_id,
                tenant_id,
                Method::POST,
                "/files",
                Some(&ct),
                Body::from(body),
            )
            .await?;
        id_of(&v)
    }

    async fn delete_file(
        &self,
        provider_id: &str,
        tenant_id: Uuid,
        file_id: &str,
    ) -> Result<(), StorageError> {
        self.storage_call(
            provider_id,
            tenant_id,
            Method::DELETE,
            &format!("/files/{file_id}"),
            None,
            Body::Empty,
        )
        .await
        .map(|_| ())
    }

    async fn create_vector_store(
        &self,
        provider_id: &str,
        tenant_id: Uuid,
        name: &str,
    ) -> Result<String, StorageError> {
        let v = self
            .storage_json(
                provider_id,
                tenant_id,
                Method::POST,
                "/vector_stores",
                &json!({ "name": name }),
            )
            .await?;
        id_of(&v)
    }

    async fn add_file_to_vector_store(
        &self,
        provider_id: &str,
        tenant_id: Uuid,
        vector_store_id: &str,
        file_id: &str,
        attributes: BTreeMap<String, String>,
    ) -> Result<VectorFileStatus, StorageError> {
        let mut body = json!({ "file_id": file_id });
        if !attributes.is_empty() {
            body["attributes"] = json!(attributes);
        }
        let v = self
            .storage_json(
                provider_id,
                tenant_id,
                Method::POST,
                &format!("/vector_stores/{vector_store_id}/files"),
                &body,
            )
            .await?;
        Ok(parse_vector_file_status(&v))
    }

    async fn get_vector_store_file_status(
        &self,
        provider_id: &str,
        tenant_id: Uuid,
        vector_store_id: &str,
        file_id: &str,
    ) -> Result<VectorFileStatus, StorageError> {
        let v = self
            .storage_call(
                provider_id,
                tenant_id,
                Method::GET,
                &format!("/vector_stores/{vector_store_id}/files/{file_id}"),
                None,
                Body::Empty,
            )
            .await?;
        Ok(parse_vector_file_status(&v))
    }

    async fn delete_vector_store(
        &self,
        provider_id: &str,
        tenant_id: Uuid,
        vector_store_id: &str,
    ) -> Result<(), StorageError> {
        self.storage_call(
            provider_id,
            tenant_id,
            Method::DELETE,
            &format!("/vector_stores/{vector_store_id}"),
            None,
            Body::Empty,
        )
        .await
        .map(|_| ())
    }
}
