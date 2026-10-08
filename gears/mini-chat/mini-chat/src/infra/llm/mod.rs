//! `llm_provider`: provider resolution, adapters and OAGW transport
//! (ADR-0001, ADR-0002, ADR-0005).

pub mod anthropic_messages;
pub mod chat_completions;
pub mod openai_responses;
pub mod types;

use std::collections::BTreeMap;
use std::pin::Pin;
use std::sync::Arc;

use bytes::Bytes;
use futures::{Stream, StreamExt};
use http::{Method, StatusCode};
use oagw_sdk::api::{ErrorSource, ServiceGatewayClientV1};
use oagw_sdk::sse::{ServerEvent, ServerEventsResponse, ServerEventsStream};
use oagw_sdk::{Body, MultipartBody, Part};
use serde_json::{Value, json};
use toolkit_canonical_errors::CanonicalError;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use self::openai_responses::{ResponsesFlavor, ResponsesParser};
use self::types::{LlmRequest, ProviderEvent, ProviderFailureKind, ProviderUsage};
use crate::config::{ProviderEntry, ProviderKind, StorageKind};
use crate::domain::error::DomainError;
use crate::domain::sanitize::sanitize_provider_message;

/// Boxed provider event stream.
pub type ProviderStream = Pin<Box<dyn Stream<Item = ProviderEvent> + Send>>;

/// A provider entry resolved for one tenant.
#[derive(Debug, Clone)]
pub struct ResolvedProvider {
    pub provider_id: String,
    pub kind: ProviderKind,
    pub alias: String,
    pub api_path: String,
}

/// File / vector-store target resolved for one tenant.
#[derive(Debug, Clone)]
pub struct StorageTarget {
    /// Provider entry id serving the storage.
    pub provider_id: String,
    /// Label stored in `attachments.storage_backend` / `chat_vector_stores.provider`.
    pub backend: String,
    pub storage_kind: StorageKind,
    pub alias: String,
    pub api_version: Option<String>,
}

impl StorageTarget {
    fn prefix(&self) -> &'static str {
        match self.storage_kind {
            StorageKind::Openai => "/v1",
            StorageKind::Azure => "/openai",
        }
    }

    fn uri(&self, path: &str) -> String {
        let mut u = format!("/{}{}{}", self.alias, self.prefix(), path);
        if let (StorageKind::Azure, Some(v)) = (self.storage_kind, &self.api_version) {
            u.push_str(if u.contains('?') { "&" } else { "?" });
            u.push_str("api-version=");
            u.push_str(v);
        }
        u
    }
}

/// Storage call failure.
#[derive(Debug, Clone)]
pub struct StorageError {
    pub transient: bool,
    pub status: Option<u16>,
    pub message: String,
}

impl std::fmt::Display for StorageError {
    #[allow(clippy::use_debug, reason = "keeps the established `Some(..)`/`None` status rendering")]
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "storage error (status {:?}, transient {}): {}", self.status, self.transient, self.message)
    }
}

/// Non-streaming call failure.
#[derive(Debug, Clone)]
pub struct CallError {
    pub kind: ProviderFailureKind,
    pub message: String,
    /// The provider reported a context-length error.
    pub context_length: bool,
}

/// Gateway to the LLM / RAG providers through OAGW.
pub struct LlmGateway {
    providers: BTreeMap<String, ProviderEntry>,
    oagw: Arc<dyn ServiceGatewayClientV1>,
    /// S2S context that provisioned the upstreams (fallback when the
    /// caller's tenant cannot see them).
    fallback_ctx: std::sync::OnceLock<SecurityContext>,
}

fn clone_request(req: &http::Request<Body>) -> Option<http::Request<Body>> {
    let body = match req.body() {
        Body::Empty => Body::Empty,
        Body::Bytes(b) => Body::Bytes(b.clone()),
        Body::Stream(_) => return None,
    };
    let mut b = http::Request::builder().method(req.method().clone()).uri(req.uri().clone());
    for (k, v) in req.headers() {
        b = b.header(k, v);
    }
    b.body(body).ok()
}

fn tenant_key(tenant_id: Uuid) -> String {
    tenant_id.to_string()
}

impl LlmGateway {
    #[must_use]
    pub fn new(providers: BTreeMap<String, ProviderEntry>, oagw: Arc<dyn ServiceGatewayClientV1>) -> Self {
        Self { providers, oagw, fallback_ctx: std::sync::OnceLock::new() }
    }

    /// Install the S2S context used as the upstream-visibility fallback.
    pub fn set_fallback_ctx(&self, ctx: SecurityContext) {
        if self.fallback_ctx.set(ctx).is_err() {
            tracing::debug!("fallback S2S context already set");
        }
    }

    /// Proxy through OAGW with the caller's context; when the caller's tenant
    /// cannot resolve the provider upstream (provisioned under the gear's S2S
    /// tenant), retry once with the S2S context.
    async fn proxy(&self, ctx: &SecurityContext, req: http::Request<Body>) -> Result<http::Response<Body>, CanonicalError> {
        let backup = clone_request(&req);
        match self.oagw.proxy_request(ctx.clone(), req).await {
            Err(e @ CanonicalError::NotFound { .. }) => {
                if let (Some(sys), Some(r)) = (self.fallback_ctx.get(), backup)
                    && sys.subject_tenant_id() != ctx.subject_tenant_id()
                {
                    tracing::debug!(error = %e, "upstream not visible to the caller tenant; using the gear context");
                    return self.oagw.proxy_request(sys.clone(), r).await;
                }
                Err(e)
            }
            other => other,
        }
    }

    #[must_use]
    pub fn providers(&self) -> &BTreeMap<String, ProviderEntry> {
        &self.providers
    }

    fn alias_for(entry: &ProviderEntry, tenant_id: Uuid) -> String {
        if let Some(ov) = entry.tenant_overrides.get(&tenant_key(tenant_id))
            && let Some(a) = ov.upstream_alias.as_ref().or(ov.host.as_ref())
        {
            return a.clone();
        }
        entry.upstream_alias.clone().unwrap_or_else(|| entry.host.clone())
    }

    /// Resolve the chat provider of a catalog model for a tenant.
    ///
    /// # Errors
    /// `Internal` when the provider id is unknown.
    pub fn resolve(&self, provider_id: &str, tenant_id: Uuid) -> Result<ResolvedProvider, DomainError> {
        let entry = self
            .providers
            .get(provider_id)
            .ok_or_else(|| DomainError::Internal(format!("unknown provider `{provider_id}`")))?;
        Ok(ResolvedProvider {
            provider_id: provider_id.to_owned(),
            kind: entry.kind,
            alias: Self::alias_for(entry, tenant_id),
            api_path: entry.api_path.clone(),
        })
    }

    /// Storage target of a provider (its `rag_provider` or itself).
    ///
    /// # Errors
    /// `Internal` when no storage-capable entry is configured.
    pub fn storage_for(&self, provider_id: &str, tenant_id: Uuid) -> Result<StorageTarget, DomainError> {
        let entry = self
            .providers
            .get(provider_id)
            .ok_or_else(|| DomainError::Internal(format!("unknown provider `{provider_id}`")))?;
        let storage_id = entry.rag_provider.clone().unwrap_or_else(|| provider_id.to_owned());
        self.storage_by_provider_id(&storage_id, tenant_id)
    }

    fn storage_by_provider_id(&self, storage_id: &str, tenant_id: Uuid) -> Result<StorageTarget, DomainError> {
        let entry = self
            .providers
            .get(storage_id)
            .ok_or_else(|| DomainError::Internal(format!("unknown rag provider `{storage_id}`")))?;
        let storage_kind = entry
            .storage_kind
            .ok_or_else(|| DomainError::Internal(format!("provider `{storage_id}` has no storage_kind")))?;
        Ok(StorageTarget {
            provider_id: storage_id.to_owned(),
            backend: entry.storage_backend.clone().unwrap_or_else(|| storage_id.to_owned()),
            storage_kind,
            alias: Self::alias_for(entry, tenant_id),
            api_version: entry.api_version.clone(),
        })
    }

    /// Map a stored backend label back to a storage target.
    ///
    /// # Errors
    /// `Internal` when no provider matches the label.
    pub fn storage_by_backend(&self, backend: &str, tenant_id: Uuid) -> Result<StorageTarget, DomainError> {
        if self.providers.contains_key(backend) {
            return self.storage_by_provider_id(backend, tenant_id);
        }
        let id = self
            .providers
            .iter()
            .find(|(_, e)| e.storage_backend.as_deref() == Some(backend))
            .map(|(id, _)| id.clone())
            .ok_or_else(|| DomainError::Internal(format!("unknown storage backend `{backend}`")))?;
        self.storage_by_provider_id(&id, tenant_id)
    }

    fn chat_uri(provider: &ResolvedProvider, provider_model_id: &str) -> String {
        format!("/{}{}", provider.alias, provider.api_path.replace("{model}", provider_model_id))
    }

    fn build_body(provider: &ResolvedProvider, req: &LlmRequest) -> Value {
        match provider.kind {
            ProviderKind::OpenaiResponses => openai_responses::build_body(req, ResponsesFlavor::OPENAI),
            ProviderKind::VllmResponses => openai_responses::build_body(req, ResponsesFlavor::VLLM),
            ProviderKind::OpenaiChatCompletions => chat_completions::build_body(req),
            ProviderKind::AnthropicMessages => anthropic_messages::build_body(req),
        }
    }

    async fn post_json(
        &self,
        ctx: &SecurityContext,
        uri: &str,
        body: &Value,
        extra_headers: &[(&'static str, &'static str)],
    ) -> Result<http::Response<Body>, CanonicalError> {
        let bytes = serde_json::to_vec(body).unwrap_or_default();
        let mut b = http::Request::builder()
            .method(Method::POST)
            .uri(uri)
            .header(http::header::CONTENT_TYPE, "application/json");
        for (k, v) in extra_headers {
            b = b.header(*k, *v);
        }
        let req = b
            .body(Body::from(bytes))
            .map_err(|e| CanonicalError::internal(format!("build request: {e}")).create())?;
        self.proxy(ctx, req).await
    }

    /// Start a streaming chat call. The returned stream yields translated
    /// provider events; a transport failure is a single `Failed` event.
    pub async fn stream_chat(&self, ctx: &SecurityContext, provider: &ResolvedProvider, req: &LlmRequest) -> ProviderStream {
        let uri = Self::chat_uri(provider, &req.provider_model_id);
        let body = Self::build_body(provider, req);
        let headers: &[(&str, &str)] = if provider.kind == ProviderKind::AnthropicMessages {
            &[("anthropic-version", "2023-06-01")]
        } else {
            &[]
        };
        let resp = match self.post_json(ctx, &uri, &body, headers).await {
            Ok(r) => r,
            Err(e) => return single(failure_from_canonical(&e)),
        };
        let status = resp.status();
        let source = resp.extensions().get::<ErrorSource>().copied();
        let is_sse = oagw_sdk::sse::is_server_events_response(resp.headers());
        if !status.is_success() || !is_sse {
            let retry_after = resp
                .headers()
                .get(http::header::RETRY_AFTER)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.trim().parse::<u64>().ok());
            let bytes = resp.into_body().into_bytes().await.unwrap_or_default();
            if status.is_success() {
                return single(ProviderEvent::Failed {
                    kind: ProviderFailureKind::ProviderError,
                    message: "Provider returned an invalid response".to_owned(),
                    provider_code: None,
                    usage: None,
                    response_id: None,
                });
            }
            let f = classify_http_failure(status, source, retry_after, &bytes);
            return single(ProviderEvent::Failed {
                kind: f.kind,
                message: f.message,
                provider_code: None,
                usage: None,
                response_id: None,
            });
        }
        let kind = provider.kind;
        match ServerEventsStream::from_response::<ServerEvent>(resp) {
            ServerEventsResponse::Events(events) => translate_stream(kind, events),
            ServerEventsResponse::Response(_) => single(ProviderEvent::Failed {
                kind: ProviderFailureKind::ProviderError,
                message: "Provider returned an invalid response".to_owned(),
                provider_code: None,
                usage: None,
                response_id: None,
            }),
        }
    }

    /// Non-streaming chat call (thread summary): `(text, usage)`.
    ///
    /// # Errors
    /// [`CallError`] on any failure.
    pub async fn complete(
        &self,
        ctx: &SecurityContext,
        provider: &ResolvedProvider,
        req: &LlmRequest,
    ) -> Result<(String, Option<ProviderUsage>), CallError> {
        let uri = Self::chat_uri(provider, &req.provider_model_id);
        let body = Self::build_body(provider, req);
        let headers: &[(&str, &str)] = if provider.kind == ProviderKind::AnthropicMessages {
            &[("anthropic-version", "2023-06-01")]
        } else {
            &[]
        };
        let resp = self.post_json(ctx, &uri, &body, headers).await.map_err(|e| {
            let f = failure_from_canonical(&e);
            match f {
                ProviderEvent::Failed { kind, message, .. } => CallError { kind, message, context_length: false },
                _ => CallError { kind: ProviderFailureKind::ProviderError, message: e.to_string(), context_length: false },
            }
        })?;
        let status = resp.status();
        let source = resp.extensions().get::<ErrorSource>().copied();
        let bytes = resp.into_body().into_bytes().await.map_err(|e| CallError {
            kind: ProviderFailureKind::ProviderError,
            message: format!("read body: {e}"),
            context_length: false,
        })?;
        if !status.is_success() {
            let raw = String::from_utf8_lossy(&bytes).to_lowercase();
            let mut f = classify_http_failure(status, source, None, &bytes);
            f.context_length = raw.contains("context_length") || raw.contains("context length") || raw.contains("maximum context");
            return Err(f);
        }
        let v: Value = serde_json::from_slice(&bytes).map_err(|e| CallError {
            kind: ProviderFailureKind::ProviderError,
            message: format!("invalid JSON response: {e}"),
            context_length: false,
        })?;
        Ok(match provider.kind {
            ProviderKind::OpenaiResponses | ProviderKind::VllmResponses => openai_responses::parse_non_streaming(&v),
            ProviderKind::OpenaiChatCompletions => chat_completions::parse_non_streaming(&v),
            ProviderKind::AnthropicMessages => anthropic_messages::parse_non_streaming(&v),
        })
    }

    async fn send(
        &self,
        ctx: &SecurityContext,
        method: Method,
        uri: String,
        body: Option<Value>,
    ) -> Result<(StatusCode, Option<ErrorSource>, Bytes), StorageError> {
        let mut b = http::Request::builder().method(method).uri(uri);
        let body = match body {
            Some(v) => {
                b = b.header(http::header::CONTENT_TYPE, "application/json");
                Body::from(serde_json::to_vec(&v).unwrap_or_default())
            }
            None => Body::Empty,
        };
        let req = b.body(body).map_err(|e| StorageError { transient: false, status: None, message: e.to_string() })?;
        let resp = self.proxy(ctx, req).await.map_err(|e| storage_err_from_canonical(&e))?;
        let status = resp.status();
        let source = resp.extensions().get::<ErrorSource>().copied();
        let bytes = resp
            .into_body()
            .into_bytes()
            .await
            .map_err(|e| StorageError { transient: true, status: Some(status.as_u16()), message: e.to_string() })?;
        Ok((status, source, bytes))
    }

    /// Upload a file to the provider Files API (`purpose=assistants`).
    ///
    /// # Errors
    /// [`StorageError`].
    pub async fn upload_file(
        &self,
        ctx: &SecurityContext,
        target: &StorageTarget,
        filename: &str,
        content_type: &str,
        data: Bytes,
    ) -> Result<String, StorageError> {
        let req = MultipartBody::new()
            .text("purpose", "assistants")
            .part(Part::bytes("file", data).filename(filename.to_owned()).content_type(content_type.to_owned()))
            .into_request("POST", target.uri("/files"))
            .map_err(|e| StorageError { transient: false, status: None, message: e.to_string() })?;
        let resp = self.proxy(ctx, req).await.map_err(|e| storage_err_from_canonical(&e))?;
        let status = resp.status();
        let bytes = resp.into_body().into_bytes().await.unwrap_or_default();
        if !status.is_success() {
            return Err(StorageError {
                transient: status.is_server_error(),
                status: Some(status.as_u16()),
                message: provider_message(&bytes),
            });
        }
        let v: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        v.get("id")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| StorageError { transient: false, status: Some(status.as_u16()), message: "file upload response without id".to_owned() })
    }

    /// Delete a provider file; 2xx and 404 are success.
    ///
    /// # Errors
    /// [`StorageError`] on any other status.
    pub async fn delete_file(&self, ctx: &SecurityContext, target: &StorageTarget, file_id: &str) -> Result<(), StorageError> {
        let (status, _, bytes) = self.send(ctx, Method::DELETE, target.uri(&format!("/files/{file_id}")), None).await?;
        if status.is_success() || status == StatusCode::NOT_FOUND {
            return Ok(());
        }
        Err(StorageError { transient: true, status: Some(status.as_u16()), message: provider_message(&bytes) })
    }

    /// Create a vector store.
    ///
    /// # Errors
    /// [`StorageError`].
    pub async fn create_vector_store(&self, ctx: &SecurityContext, target: &StorageTarget, name: &str) -> Result<String, StorageError> {
        let (status, _, bytes) = self
            .send(ctx, Method::POST, target.uri("/vector_stores"), Some(json!({"name": name})))
            .await?;
        if !status.is_success() {
            return Err(StorageError { transient: true, status: Some(status.as_u16()), message: provider_message(&bytes) });
        }
        let v: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        v.get("id").and_then(Value::as_str).map(str::to_owned).ok_or_else(|| StorageError {
            transient: false,
            status: Some(status.as_u16()),
            message: "vector store response without id".to_owned(),
        })
    }

    /// Add a file to a vector store; returns the reported status.
    ///
    /// # Errors
    /// [`StorageError`].
    pub async fn add_vector_store_file(
        &self,
        ctx: &SecurityContext,
        target: &StorageTarget,
        vector_store_id: &str,
        file_id: &str,
        attachment_id: Uuid,
    ) -> Result<Option<String>, StorageError> {
        let body = json!({"file_id": file_id, "attributes": {"attachment_id": attachment_id.to_string()}});
        let (status, _, bytes) = self
            .send(ctx, Method::POST, target.uri(&format!("/vector_stores/{vector_store_id}/files")), Some(body))
            .await?;
        if !status.is_success() {
            return Err(StorageError { transient: false, status: Some(status.as_u16()), message: provider_message(&bytes) });
        }
        let v: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        Ok(v.get("status").and_then(Value::as_str).map(str::to_owned))
    }

    /// Read the indexing status of a vector-store file (`None` = no status).
    ///
    /// # Errors
    /// [`StorageError`] (`transient` for 5xx / gateway failures).
    pub async fn vector_store_file_status(
        &self,
        ctx: &SecurityContext,
        target: &StorageTarget,
        vector_store_id: &str,
        file_id: &str,
    ) -> Result<Option<String>, StorageError> {
        let (status, source, bytes) = self
            .send(ctx, Method::GET, target.uri(&format!("/vector_stores/{vector_store_id}/files/{file_id}")), None)
            .await?;
        if !status.is_success() {
            return Err(StorageError {
                transient: status.is_server_error() || source == Some(ErrorSource::Gateway),
                status: Some(status.as_u16()),
                message: provider_message(&bytes),
            });
        }
        let v: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        Ok(v.get("status").and_then(Value::as_str).map(str::to_owned))
    }

    /// Delete a vector store; 2xx and 404 are success.
    ///
    /// # Errors
    /// [`StorageError`] on any other status.
    pub async fn delete_vector_store(&self, ctx: &SecurityContext, target: &StorageTarget, vector_store_id: &str) -> Result<(), StorageError> {
        let (status, _, bytes) = self
            .send(ctx, Method::DELETE, target.uri(&format!("/vector_stores/{vector_store_id}")), None)
            .await?;
        if status.is_success() || status == StatusCode::NOT_FOUND {
            return Ok(());
        }
        Err(StorageError { transient: true, status: Some(status.as_u16()), message: provider_message(&bytes) })
    }

    /// Raw POST through OAGW (knowledge search retriever).
    ///
    /// # Errors
    /// [`StorageError`].
    pub async fn post_raw(&self, ctx: &SecurityContext, uri: String, body: Value) -> Result<Value, StorageError> {
        let (status, _, bytes) = self.send(ctx, Method::POST, uri, Some(body)).await?;
        if !status.is_success() {
            return Err(StorageError { transient: status.is_server_error(), status: Some(status.as_u16()), message: provider_message(&bytes) });
        }
        Ok(serde_json::from_slice(&bytes).unwrap_or(Value::Null))
    }

    /// Anthropic Files upload (secondary image copy; `file` part only).
    ///
    /// # Errors
    /// [`StorageError`].
    pub async fn upload_anthropic_file(
        &self,
        ctx: &SecurityContext,
        alias: &str,
        filename: &str,
        content_type: &str,
        data: Bytes,
    ) -> Result<String, StorageError> {
        let req = MultipartBody::new()
            .part(Part::bytes("file", data).filename(filename.to_owned()).content_type(content_type.to_owned()))
            .into_request("POST", format!("/{alias}/v1/files"))
            .map_err(|e| StorageError { transient: false, status: None, message: e.to_string() })?;
        let resp = self.proxy(ctx, req).await.map_err(|e| storage_err_from_canonical(&e))?;
        let status = resp.status();
        let bytes = resp.into_body().into_bytes().await.unwrap_or_default();
        if !status.is_success() {
            return Err(StorageError { transient: status.is_server_error(), status: Some(status.as_u16()), message: provider_message(&bytes) });
        }
        let v: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        v.get("id").and_then(Value::as_str).map(str::to_owned).ok_or_else(|| StorageError {
            transient: false,
            status: Some(status.as_u16()),
            message: "anthropic file upload without id".to_owned(),
        })
    }

    /// Anthropic Files delete; 2xx and 404 are success.
    ///
    /// # Errors
    /// [`StorageError`].
    pub async fn delete_anthropic_file(&self, ctx: &SecurityContext, alias: &str, file_id: &str) -> Result<(), StorageError> {
        let (status, _, bytes) = self.send(ctx, Method::DELETE, format!("/{alias}/v1/files/{file_id}"), None).await?;
        if status.is_success() || status == StatusCode::NOT_FOUND {
            return Ok(());
        }
        Err(StorageError { transient: true, status: Some(status.as_u16()), message: provider_message(&bytes) })
    }
}

fn single(ev: ProviderEvent) -> ProviderStream {
    Box::pin(futures::stream::iter(vec![ev]))
}

fn translate_stream(kind: ProviderKind, events: ServerEventsStream<ServerEvent>) -> ProviderStream {
    enum Parser {
        Responses(ResponsesParser),
        Chat(chat_completions::ChatCompletionsParser),
        Anthropic(anthropic_messages::AnthropicParser),
    }
    let parser = match kind {
        ProviderKind::OpenaiResponses => Parser::Responses(ResponsesParser::new(false)),
        ProviderKind::VllmResponses => Parser::Responses(ResponsesParser::new(true)),
        ProviderKind::OpenaiChatCompletions => Parser::Chat(chat_completions::ChatCompletionsParser::default()),
        ProviderKind::AnthropicMessages => Parser::Anthropic(anthropic_messages::AnthropicParser::default()),
    };
    let state = (events, parser, false);
    let s = futures::stream::unfold(state, |(mut events, mut parser, done)| async move {
        if done {
            return None;
        }
        loop {
            match events.next().await {
                None => {
                    // End of provider stream: flush adapters that finalize at EOF.
                    let tail = match &mut parser {
                        Parser::Chat(p) => p.finish(),
                        Parser::Responses(_) | Parser::Anthropic(_) => Vec::new(),
                    };
                    if tail.is_empty() {
                        return None;
                    }
                    return Some((tail, (events, parser, true)));
                }
                Some(Err(e)) => {
                    let ev = ProviderEvent::Failed {
                        kind: ProviderFailureKind::ProviderError,
                        message: sanitize_provider_message(&format!("Provider stream failed: {e}")),
                        provider_code: None,
                        usage: None,
                        response_id: None,
                    };
                    return Some((vec![ev], (events, parser, true)));
                }
                Some(Ok(ev)) => {
                    let out = match &mut parser {
                        Parser::Responses(p) => p.on_event(ev.event.as_deref(), &ev.data),
                        Parser::Chat(p) => p.on_event(&ev.data),
                        Parser::Anthropic(p) => p.on_event(ev.event.as_deref(), &ev.data),
                    };
                    if !out.is_empty() {
                        return Some((out, (events, parser, false)));
                    }
                }
            }
        }
    });
    Box::pin(s.flat_map(futures::stream::iter))
}

fn provider_message(bytes: &[u8]) -> String {
    let v: Value = serde_json::from_slice(bytes).unwrap_or(Value::Null);
    let (_, msg) = openai_responses::extract_error(&v);
    let msg = msg
        .or_else(|| v.get("detail").and_then(Value::as_str).map(str::to_owned))
        .unwrap_or_else(|| String::from_utf8_lossy(bytes).chars().take(500).collect());
    sanitize_provider_message(&msg)
}

fn classify_http_failure(status: StatusCode, source: Option<ErrorSource>, retry_after: Option<u64>, bytes: &[u8]) -> CallError {
    if source == Some(ErrorSource::Gateway) {
        let is_timeout = status == StatusCode::GATEWAY_TIMEOUT
            || String::from_utf8_lossy(bytes).contains("deadline_exceeded");
        if is_timeout {
            return CallError {
                kind: ProviderFailureKind::ProviderTimeout,
                message: "Provider request timed out".to_owned(),
                context_length: false,
            };
        }
        return CallError {
            kind: ProviderFailureKind::ProviderError,
            message: "Provider is currently unavailable".to_owned(),
            context_length: false,
        };
    }
    if status == StatusCode::TOO_MANY_REQUESTS {
        let message = match retry_after {
            Some(n) => format!("Provider rate limit exceeded, retry in {n}s"),
            None => "Provider rate limit exceeded".to_owned(),
        };
        return CallError { kind: ProviderFailureKind::RateLimited, message, context_length: false };
    }
    let msg = provider_message(bytes);
    let message = if msg.trim().is_empty() { format!("Provider returned HTTP {}", status.as_u16()) } else { msg };
    CallError { kind: ProviderFailureKind::ProviderError, message, context_length: false }
}

fn failure_from_canonical(e: &CanonicalError) -> ProviderEvent {
    let (kind, message) = match e {
        CanonicalError::DeadlineExceeded { .. } => {
            (ProviderFailureKind::ProviderTimeout, "Provider request timed out".to_owned())
        }
        CanonicalError::ResourceExhausted { .. } => {
            (ProviderFailureKind::RateLimited, "Provider rate limit exceeded".to_owned())
        }
        _ => {
            tracing::warn!(error = %e, "provider call failed in the gateway");
            (ProviderFailureKind::ProviderError, "Provider is currently unavailable".to_owned())
        }
    };
    ProviderEvent::Failed { kind, message, provider_code: None, usage: None, response_id: None }
}

fn storage_err_from_canonical(e: &CanonicalError) -> StorageError {
    let transient = matches!(
        e,
        CanonicalError::DeadlineExceeded { .. } | CanonicalError::ServiceUnavailable { .. } | CanonicalError::Internal { .. }
    );
    StorageError { transient, status: Some(e.status_code()), message: e.to_string() }
}

/// Internal: the `CallError` of a non-streaming call -> the matching streaming failure.
impl From<CallError> for ProviderEvent {
    fn from(e: CallError) -> Self {
        Self::Failed { kind: e.kind, message: e.message, provider_code: None, usage: None, response_id: None }
    }
}
