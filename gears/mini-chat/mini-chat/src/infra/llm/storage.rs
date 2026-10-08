//! Provider Files API and Vector Stores API client (`OpenAI` / Azure `OpenAI`),
//! dispatched by `storage_kind` through [`StorageTarget`].

use std::sync::Arc;

use bytes::Bytes;
use oagw_sdk::{Body, MultipartBody, Part};
use serde_json::{Value, json};

use super::client::ProxyClient;
use super::resolver::{KnowledgeTarget, StorageTarget};

/// One knowledge-base search result chunk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KnowledgeChunk {
    pub filename: String,
    pub text: String,
}

/// Storage call failure.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum StorageError {
    /// Provider answered with a non-2xx status.
    #[error("provider status {status}: {message}")]
    Http { status: u16, message: String },
    /// Gateway or transport failure (transient).
    #[error("gateway: {0}")]
    Gateway(String),
    /// Malformed provider response.
    #[error("invalid response: {0}")]
    Invalid(String),
}

impl StorageError {
    /// Transient failures keep a status poll going.
    #[must_use]
    pub fn is_transient(&self) -> bool {
        match self {
            Self::Gateway(_) => true,
            Self::Http { status, .. } => *status >= 500 || *status == 429,
            Self::Invalid(_) => false,
        }
    }
}

/// Files / vector stores client.
#[derive(Clone)]
pub struct StorageClient {
    proxy: Arc<dyn ProxyClient>,
}

async fn json_response(
    proxy: &dyn ProxyClient,
    req: http::Request<Body>,
) -> Result<(u16, Value), StorageError> {
    let resp = proxy
        .proxy(req)
        .await
        .map_err(|e| StorageError::Gateway(e.to_string()))?;
    let status = resp.status().as_u16();
    let bytes = resp
        .into_body()
        .into_bytes()
        .await
        .map_err(|e| StorageError::Gateway(e.to_string()))?;
    let v: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    Ok((status, v))
}

fn ok_or_http(status: u16, v: &Value) -> Result<(), StorageError> {
    if (200..300).contains(&status) {
        Ok(())
    } else {
        let message = v
            .pointer("/error/message")
            .and_then(Value::as_str)
            .unwrap_or("provider error")
            .to_owned();
        Err(StorageError::Http { status, message })
    }
}

fn json_request(method: http::Method, uri: &str, body: Option<&Value>) -> Result<http::Request<Body>, StorageError> {
    let mut b = http::Request::builder().method(method).uri(uri);
    let body = match body {
        Some(v) => {
            b = b.header(http::header::CONTENT_TYPE, "application/json");
            Body::from(serde_json::to_vec(v).map_err(|e| StorageError::Invalid(e.to_string()))?)
        }
        None => Body::Empty,
    };
    b.body(body).map_err(|e| StorageError::Invalid(e.to_string()))
}

impl StorageClient {
    #[must_use]
    pub fn new(proxy: Arc<dyn ProxyClient>) -> Self {
        Self { proxy }
    }

    /// Knowledge retriever (Azure `OpenAI` vector store search through OAGW).
    ///
    /// # Errors
    /// Provider or gateway failure.
    pub async fn search_knowledge(
        &self,
        target: &KnowledgeTarget,
        query: &str,
        top_k: usize,
    ) -> Result<Vec<KnowledgeChunk>, StorageError> {
        let body = json!({ "query": query, "max_num_results": top_k });
        let req = json_request(http::Method::POST, &target.search_uri(), Some(&body))?;
        let (status, v) = json_response(self.proxy.as_ref(), req).await?;
        ok_or_http(status, &v)?;
        let data = v
            .get("data")
            .and_then(Value::as_array)
            .ok_or_else(|| StorageError::Invalid("search response without data".into()))?;
        Ok(data
            .iter()
            .take(top_k)
            .map(|d| {
                let text: String = d
                    .get("content")
                    .and_then(Value::as_array)
                    .map(|parts| {
                        parts
                            .iter()
                            .filter_map(|p| p.get("text").and_then(Value::as_str))
                            .collect::<Vec<_>>()
                            .join("\n")
                    })
                    .unwrap_or_default();
                KnowledgeChunk {
                    filename: d.get("filename").and_then(Value::as_str).unwrap_or_default().to_owned(),
                    text: text.chars().take(target.max_chunk_chars).collect(),
                }
            })
            .collect())
    }

    /// `POST {prefix}/files` with `purpose=assistants`; returns the file id.
    ///
    /// # Errors
    /// Provider or gateway failure.
    pub async fn upload_file(
        &self,
        target: &StorageTarget,
        provider_filename: &str,
        content_type: &str,
        data: Bytes,
    ) -> Result<String, StorageError> {
        let mp = MultipartBody::new()
            .text("purpose", "assistants")
            .part(
                Part::bytes("file", data)
                    .filename(provider_filename)
                    .content_type(content_type),
            );
        let req = mp
            .into_request(http::Method::POST, target.uri("/files"))
            .map_err(|e| StorageError::Invalid(e.to_string()))?;
        let (status, v) = json_response(self.proxy.as_ref(), req).await?;
        ok_or_http(status, &v)?;
        v.get("id")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned)
            .ok_or_else(|| StorageError::Invalid("file id missing".into()))
    }

    /// Upload to the Anthropic Files API (`POST {alias}/v1/files`, `file` part only).
    ///
    /// # Errors
    /// Provider or gateway failure.
    pub async fn upload_anthropic_file(
        &self,
        alias: &str,
        filename: &str,
        content_type: &str,
        data: Bytes,
    ) -> Result<String, StorageError> {
        let mp = MultipartBody::new().part(
            Part::bytes("file", data)
                .filename(filename)
                .content_type(content_type),
        );
        let req = mp
            .into_request(http::Method::POST, format!("/{alias}/v1/files"))
            .map_err(|e| StorageError::Invalid(e.to_string()))?;
        let (status, v) = json_response(self.proxy.as_ref(), req).await?;
        ok_or_http(status, &v)?;
        v.get("id")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned)
            .ok_or_else(|| StorageError::Invalid("file id missing".into()))
    }

    /// Delete an Anthropic Files API file (2xx / 404 = success).
    ///
    /// # Errors
    /// Provider or gateway failure.
    pub async fn delete_anthropic_file(&self, alias: &str, file_id: &str) -> Result<(), StorageError> {
        let req = json_request(http::Method::DELETE, &format!("/{alias}/v1/files/{file_id}"), None)?;
        let (status, v) = json_response(self.proxy.as_ref(), req).await?;
        if status == 404 {
            return Ok(());
        }
        ok_or_http(status, &v)
    }

    /// `DELETE {prefix}/files/{id}`; 2xx and 404 count as success.
    ///
    /// # Errors
    /// Any other status, or gateway failure.
    pub async fn delete_file(&self, target: &StorageTarget, file_id: &str) -> Result<(), StorageError> {
        let req = json_request(http::Method::DELETE, &target.uri(&format!("/files/{file_id}")), None)?;
        let (status, v) = json_response(self.proxy.as_ref(), req).await?;
        if status == 404 {
            return Ok(());
        }
        ok_or_http(status, &v)
    }

    /// `POST {prefix}/vector_stores`; returns the vector store id.
    ///
    /// # Errors
    /// Provider or gateway failure.
    pub async fn create_vector_store(&self, target: &StorageTarget, name: &str) -> Result<String, StorageError> {
        let req = json_request(
            http::Method::POST,
            &target.uri("/vector_stores"),
            Some(&json!({ "name": name })),
        )?;
        let (status, v) = json_response(self.proxy.as_ref(), req).await?;
        ok_or_http(status, &v)?;
        v.get("id")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned)
            .ok_or_else(|| StorageError::Invalid("vector store id missing".into()))
    }

    /// `POST {prefix}/vector_stores/{vs}/files`; returns the indexing status
    /// (`None` when the response has no status).
    ///
    /// # Errors
    /// Provider or gateway failure.
    pub async fn add_file_to_vector_store(
        &self,
        target: &StorageTarget,
        vector_store_id: &str,
        file_id: &str,
        attachment_id: &str,
    ) -> Result<Option<String>, StorageError> {
        let req = json_request(
            http::Method::POST,
            &target.uri(&format!("/vector_stores/{vector_store_id}/files")),
            Some(&json!({ "file_id": file_id, "attributes": { "attachment_id": attachment_id } })),
        )?;
        let (status, v) = json_response(self.proxy.as_ref(), req).await?;
        ok_or_http(status, &v)?;
        Ok(v.get("status").and_then(Value::as_str).map(ToOwned::to_owned))
    }

    /// `GET {prefix}/vector_stores/{vs}/files/{file_id}` → status.
    ///
    /// # Errors
    /// Provider or gateway failure.
    pub async fn get_vector_store_file_status(
        &self,
        target: &StorageTarget,
        vector_store_id: &str,
        file_id: &str,
    ) -> Result<Option<String>, StorageError> {
        let req = json_request(
            http::Method::GET,
            &target.uri(&format!("/vector_stores/{vector_store_id}/files/{file_id}")),
            None,
        )?;
        let (status, v) = json_response(self.proxy.as_ref(), req).await?;
        ok_or_http(status, &v)?;
        Ok(v.get("status").and_then(Value::as_str).map(ToOwned::to_owned))
    }

    /// `DELETE {prefix}/vector_stores/{vs}`; 2xx and 404 count as success.
    ///
    /// # Errors
    /// Any other status, or gateway failure.
    pub async fn delete_vector_store(&self, target: &StorageTarget, vector_store_id: &str) -> Result<(), StorageError> {
        let req = json_request(
            http::Method::DELETE,
            &target.uri(&format!("/vector_stores/{vector_store_id}")),
            None,
        )?;
        let (status, v) = json_response(self.proxy.as_ref(), req).await?;
        if status == 404 {
            return Ok(());
        }
        ok_or_http(status, &v)
    }
}
