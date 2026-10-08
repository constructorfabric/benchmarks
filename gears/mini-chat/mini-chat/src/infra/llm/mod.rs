//! `llm_provider`: provider resolution, adapters and file / vector-store
//! dispatch through the in-process OAGW proxy (ADR-0001, ADR-0005).

pub mod adapters;
pub mod sse;
pub mod types;

use std::collections::{HashMap, VecDeque};
use std::pin::Pin;
use std::sync::Arc;

use bytes::Bytes;
use futures::{Stream, StreamExt};
use oagw_sdk::api::ErrorSource;
use oagw_sdk::{Body, MultipartBody, Part, ServiceGatewayClientV1};
use parking_lot::RwLock;
use serde_json::{Value, json};
use toolkit_canonical_errors::CanonicalError;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use self::adapters::{EventTranslator, build_body, error_message, parse_completion};
use self::sse::SseDecoder;
use self::types::{ChatRequest, Completion, ProviderCallError, ProviderEvent, StreamErrorCode};
use crate::config::{MiniChatConfig, ProviderEntry, ProviderKind, StorageKind};
use crate::domain::sanitize::sanitize_provider_message;

pub type ProviderStream = Pin<Box<dyn Stream<Item = ProviderEvent> + Send>>;

const MAX_ERROR_BODY: usize = 64 * 1024;

/// Resolved chat endpoint of a provider for a tenant.
#[derive(Debug, Clone)]
pub struct ChatTarget {
    pub provider_id: String,
    pub kind: ProviderKind,
    pub alias: String,
    pub api_path: String,
}

/// Resolved RAG (files / vector stores) endpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RagTarget {
    pub provider_id: String,
    pub alias: String,
    pub prefix: String,
    pub api_version: Option<String>,
    pub storage_backend: String,
}

impl RagTarget {
    fn uri(&self, path: &str) -> String {
        let q = self.api_version.as_ref().map(|v| format!("?api-version={v}")).unwrap_or_default();
        format!("/{}{}{}{}", self.alias, self.prefix, path, q)
    }
}

/// Errors of RAG operations.
#[derive(Debug, Clone, thiserror::Error)]
#[error("rag operation failed (status {status:?}): {message}")]
pub struct RagError {
    pub status: Option<u16>,
    pub message: String,
    pub transient: bool,
}

/// Maps provider ids (and tenants) to OAGW aliases and paths.
pub struct ProviderResolver {
    providers: HashMap<String, ProviderEntry>,
    /// Configured alias → alias returned by OAGW (hostname upstreams on non-standard ports).
    alias_remap: RwLock<HashMap<String, String>>,
}

impl ProviderResolver {
    #[must_use]
    pub fn new(cfg: &MiniChatConfig) -> Self {
        Self { providers: cfg.providers.clone(), alias_remap: RwLock::new(HashMap::new()) }
    }

    pub fn remap_alias(&self, configured: &str, actual: &str) {
        if configured != actual {
            self.alias_remap.write().insert(configured.to_owned(), actual.to_owned());
        }
    }

    fn effective_alias(&self, alias: &str) -> String {
        self.alias_remap.read().get(alias).cloned().unwrap_or_else(|| alias.to_owned())
    }

    #[must_use]
    pub fn entry(&self, provider_id: &str) -> Option<&ProviderEntry> {
        self.providers.get(provider_id)
    }

    #[must_use]
    pub fn providers(&self) -> &HashMap<String, ProviderEntry> {
        &self.providers
    }

    fn alias_for(&self, entry: &ProviderEntry, tenant_id: Uuid) -> String {
        if let Some(o) = entry.tenant_overrides.get(&tenant_id.to_string())
            && let Some(a) = o.upstream_alias.as_ref().or(o.host.as_ref())
        {
            return self.effective_alias(a);
        }
        self.effective_alias(&entry.alias())
    }

    /// # Errors
    /// Unknown provider id.
    pub fn chat_target(&self, provider_id: &str, tenant_id: Uuid) -> Result<ChatTarget, String> {
        let entry = self.providers.get(provider_id).ok_or_else(|| format!("unknown provider '{provider_id}'"))?;
        Ok(ChatTarget {
            provider_id: provider_id.to_owned(),
            kind: entry.kind,
            alias: self.alias_for(entry, tenant_id),
            api_path: entry.api_path.clone(),
        })
    }

    /// RAG target for a chat model served by `provider_id`.
    ///
    /// # Errors
    /// Unknown provider or no storage-capable provider.
    pub fn rag_target(&self, provider_id: &str, tenant_id: Uuid) -> Result<RagTarget, String> {
        let entry = self.providers.get(provider_id).ok_or_else(|| format!("unknown provider '{provider_id}'"))?;
        let rag_id = entry.rag_provider.clone().unwrap_or_else(|| provider_id.to_owned());
        self.rag_target_of(&rag_id, tenant_id)
    }

    fn rag_target_of(&self, rag_id: &str, tenant_id: Uuid) -> Result<RagTarget, String> {
        let rag = self.providers.get(rag_id).ok_or_else(|| format!("unknown rag provider '{rag_id}'"))?;
        let kind = rag.storage_kind.ok_or_else(|| format!("provider '{rag_id}' has no storage_kind"))?;
        let (prefix, api_version) = match kind {
            StorageKind::Openai => ("/v1".to_owned(), None),
            StorageKind::Azure => ("/openai".to_owned(), rag.api_version.clone()),
        };
        Ok(RagTarget {
            provider_id: rag_id.to_owned(),
            alias: self.alias_for(rag, tenant_id),
            prefix,
            api_version,
            storage_backend: rag.storage_backend.clone().unwrap_or_else(|| rag_id.to_owned()),
        })
    }

    /// RAG target from a stored `storage_backend` label (cleanup paths).
    ///
    /// # Errors
    /// No provider maps to the label.
    pub fn rag_target_by_backend(&self, label: &str, tenant_id: Uuid) -> Result<RagTarget, String> {
        let id = self
            .providers
            .iter()
            .find(|(id, p)| p.storage_kind.is_some() && p.storage_backend.as_deref().unwrap_or(id.as_str()) == label)
            .map(|(id, _)| id.clone())
            .or_else(|| {
                let mut ids: Vec<&String> = self.providers.iter().filter(|(_, p)| p.storage_kind.is_some()).map(|(id, _)| id).collect();
                ids.sort();
                (ids.len() == 1).then(|| ids[0].clone())
            })
            .ok_or_else(|| format!("no provider for storage backend '{label}'"))?;
        self.rag_target_of(&id, tenant_id)
    }
}

/// Default platform identity used when no S2S context is available.
#[must_use]
pub fn fallback_context() -> SecurityContext {
    SecurityContext::builder()
        .subject_id(toolkit_security::constants::DEFAULT_SUBJECT_ID)
        .subject_tenant_id(toolkit_security::constants::DEFAULT_TENANT_ID)
        .token_scopes(vec!["*".to_owned()])
        .build()
        .unwrap_or_else(|_| SecurityContext::anonymous())
}

/// OAGW-backed provider gateway.
pub struct LlmGateway {
    oagw: Arc<dyn ServiceGatewayClientV1>,
    s2s: Arc<RwLock<Option<SecurityContext>>>,
    pub resolver: Arc<ProviderResolver>,
}

fn call_error(code: StreamErrorCode, message: impl Into<String>, status: Option<u16>) -> ProviderCallError {
    ProviderCallError { code, message: message.into(), status, context_length: false }
}

fn map_canonical(err: &CanonicalError) -> ProviderCallError {
    match err {
        CanonicalError::DeadlineExceeded { .. } => {
            call_error(StreamErrorCode::ProviderTimeout, "Provider request timed out", Some(504))
        }
        _ => call_error(
            StreamErrorCode::ProviderError,
            sanitize_provider_message(err.detail()),
            None,
        ),
    }
}

async fn read_limited(body: Body) -> Bytes {
    let mut stream = body.into_stream();
    let mut out = Vec::new();
    while let Some(chunk) = stream.next().await {
        match chunk {
            Ok(b) => {
                out.extend_from_slice(&b);
                if out.len() > MAX_ERROR_BODY {
                    break;
                }
            }
            Err(_) => break,
        }
    }
    Bytes::from(out)
}

async fn error_from_response(resp: http::Response<Body>) -> ProviderCallError {
    let status = resp.status().as_u16();
    let source = resp.extensions().get::<ErrorSource>().copied();
    let retry_after = resp
        .headers()
        .get(http::header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.trim().parse::<u64>().ok());
    let body = read_limited(resp.into_body()).await;
    let parsed: Option<Value> = serde_json::from_slice(&body).ok();
    let (message, ctx_len) = parsed.as_ref().map_or_else(
        || (String::from_utf8_lossy(&body).trim().to_owned(), false),
        error_message,
    );
    let message = if message.is_empty() { format!("Provider returned HTTP {status}") } else { message };
    let message = sanitize_provider_message(&message);
    if status == 429 {
        let msg = match retry_after {
            Some(n) => format!("Provider rate limit exceeded, retry in {n}s"),
            None => "Provider rate limit exceeded".to_owned(),
        };
        return call_error(StreamErrorCode::RateLimited, msg, Some(status));
    }
    let is_gateway_timeout = status == 504
        && (source == Some(ErrorSource::Gateway)
            || parsed.as_ref().and_then(|v| v.get("type")).and_then(Value::as_str).is_some_and(|t| t.contains("deadline_exceeded")));
    if is_gateway_timeout {
        return call_error(StreamErrorCode::ProviderTimeout, "Provider request timed out", Some(status));
    }
    ProviderCallError { code: StreamErrorCode::ProviderError, message, status: Some(status), context_length: ctx_len }
}

fn is_event_stream(resp: &http::Response<Body>) -> bool {
    resp.headers()
        .get(http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|ct| ct.to_ascii_lowercase().starts_with("text/event-stream"))
}

impl LlmGateway {
    #[must_use]
    pub fn new(oagw: Arc<dyn ServiceGatewayClientV1>, resolver: Arc<ProviderResolver>) -> Self {
        Self { oagw, s2s: Arc::new(RwLock::new(None)), resolver }
    }

    /// Shared slot for the S2S context obtained at start.
    #[must_use]
    pub fn s2s_slot(&self) -> Arc<RwLock<Option<SecurityContext>>> {
        Arc::clone(&self.s2s)
    }

    #[must_use]
    pub fn oagw(&self) -> Arc<dyn ServiceGatewayClientV1> {
        Arc::clone(&self.oagw)
    }

    fn ctx(&self) -> SecurityContext {
        self.s2s.read().clone().unwrap_or_else(fallback_context)
    }

    async fn send(&self, req: http::Request<Body>) -> Result<http::Response<Body>, CanonicalError> {
        self.oagw.proxy_request(self.ctx(), req).await
    }

    fn chat_request(target: &ChatTarget, req: &ChatRequest) -> Result<http::Request<Body>, ProviderCallError> {
        let path = target.api_path.replace("{model}", &req.model);
        let body = build_body(target.kind, req);
        let bytes = serde_json::to_vec(&body)
            .map_err(|e| call_error(StreamErrorCode::ProviderError, format!("serialize request: {e}"), None))?;
        let mut builder = http::Request::builder()
            .method(http::Method::POST)
            .uri(format!("/{}{}", target.alias, path))
            .header(http::header::CONTENT_TYPE, "application/json");
        if req.stream {
            builder = builder.header(http::header::ACCEPT, "text/event-stream");
        }
        if target.kind == ProviderKind::AnthropicMessages {
            builder = builder.header("anthropic-version", "2023-06-01");
        }
        builder
            .body(Body::from(bytes))
            .map_err(|e| call_error(StreamErrorCode::ProviderError, format!("build request: {e}"), None))
    }

    /// Start a streaming chat request.
    ///
    /// # Errors
    /// Gateway error or non-2xx provider status before the stream started.
    pub async fn stream_chat(&self, target: &ChatTarget, req: &ChatRequest) -> Result<ProviderStream, ProviderCallError> {
        let http_req = Self::chat_request(target, req)?;
        let resp = self.send(http_req).await.map_err(|e| map_canonical(&e))?;
        if !resp.status().is_success() {
            return Err(error_from_response(resp).await);
        }
        let kind = target.kind;
        if !is_event_stream(&resp) {
            // Provider answered with a plain JSON body; translate it.
            let body = read_limited(resp.into_body()).await;
            let v: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
            let c = parse_completion(kind, &v);
            let mut events = Vec::new();
            if !c.text.is_empty() {
                events.push(ProviderEvent::TextDelta(c.text));
            }
            events.push(ProviderEvent::Completed {
                response_id: v.get("id").and_then(Value::as_str).map(str::to_owned),
                usage: c.usage,
                annotations: Vec::new(),
                incomplete_reason: None,
            });
            return Ok(Box::pin(futures::stream::iter(events)));
        }
        Ok(translate_stream(kind, resp.into_body()))
    }

    /// Non-streaming completion (thread summary).
    ///
    /// # Errors
    /// Gateway or provider failure.
    pub async fn complete(&self, target: &ChatTarget, req: &ChatRequest) -> Result<Completion, ProviderCallError> {
        let mut req = req.clone();
        req.stream = false;
        let http_req = Self::chat_request(target, &req)?;
        let resp = self.send(http_req).await.map_err(|e| map_canonical(&e))?;
        if !resp.status().is_success() {
            return Err(error_from_response(resp).await);
        }
        let event_stream = is_event_stream(&resp);
        let body = resp
            .into_body()
            .into_bytes()
            .await
            .map_err(|e| call_error(StreamErrorCode::ProviderError, format!("read body: {e}"), None))?;
        if event_stream {
            // Tolerate an SSE answer to a non-stream request.
            let mut dec = SseDecoder::new();
            let mut tr = EventTranslator::new(target.kind);
            let mut text = String::new();
            let mut usage = None;
            let mut frames = dec.push(&body);
            frames.extend(dec.finish());
            for f in frames {
                for ev in tr.translate(&f) {
                    match ev {
                        ProviderEvent::TextDelta(t) => text.push_str(&t),
                        ProviderEvent::Completed { usage: u, .. } => usage = u,
                        ProviderEvent::Failed { code, message, .. } => return Err(call_error(code, message, None)),
                        _ => {}
                    }
                }
            }
            return Ok(Completion { text, usage });
        }
        let v: Value = serde_json::from_slice(&body)
            .map_err(|e| call_error(StreamErrorCode::ProviderError, format!("invalid response: {e}"), None))?;
        Ok(parse_completion(target.kind, &v))
    }

    // ---------------------------------------------------------------- RAG

    async fn rag_call(&self, req: http::Request<Body>) -> Result<(u16, Value), RagError> {
        let resp = self.send(req).await.map_err(|e| RagError {
            status: None,
            message: sanitize_provider_message(e.detail()),
            transient: true,
        })?;
        let status = resp.status().as_u16();
        let body = read_limited(resp.into_body()).await;
        let v: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
        Ok((status, v))
    }

    fn rag_err(status: u16, v: &Value) -> RagError {
        let (message, _) = error_message(v);
        RagError {
            status: Some(status),
            message: sanitize_provider_message(&message),
            transient: status >= 500 || status == 429 || status == 408,
        }
    }

    /// Upload a file (`purpose=assistants`).
    ///
    /// # Errors
    /// Provider or gateway failure.
    pub async fn upload_file(
        &self,
        target: &RagTarget,
        filename: &str,
        content_type: &str,
        data: Bytes,
    ) -> Result<String, RagError> {
        let req = MultipartBody::new()
            .text("purpose", "assistants")
            .part(Part::bytes("file", data).filename(filename).content_type(content_type))
            .into_request("POST", target.uri("/files"))
            .map_err(|e| RagError { status: None, message: e.to_string(), transient: false })?;
        let (status, v) = self.rag_call(req).await?;
        if !(200..300).contains(&status) {
            return Err(Self::rag_err(status, &v));
        }
        v.get("id")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| RagError { status: Some(status), message: "file id missing".into(), transient: false })
    }

    fn json_request(method: http::Method, uri: String, body: Option<&Value>) -> Result<http::Request<Body>, RagError> {
        let mut b = http::Request::builder().method(method).uri(uri);
        let body = match body {
            Some(v) => {
                b = b.header(http::header::CONTENT_TYPE, "application/json");
                Body::from(serde_json::to_vec(v).unwrap_or_default())
            }
            None => Body::Empty,
        };
        b.body(body).map_err(|e| RagError { status: None, message: e.to_string(), transient: false })
    }

    /// Delete a provider file; `Ok(false)` when it was already gone (404).
    ///
    /// # Errors
    /// Any other non-2xx status or gateway failure.
    pub async fn delete_file(&self, target: &RagTarget, file_id: &str) -> Result<bool, RagError> {
        let req = Self::json_request(http::Method::DELETE, target.uri(&format!("/files/{file_id}")), None)?;
        let (status, v) = self.rag_call(req).await?;
        match status {
            200..=299 => Ok(true),
            404 => Ok(false),
            _ => Err(Self::rag_err(status, &v)),
        }
    }

    /// Create a vector store.
    ///
    /// # Errors
    /// Provider or gateway failure.
    pub async fn create_vector_store(&self, target: &RagTarget, name: &str) -> Result<String, RagError> {
        let req = Self::json_request(http::Method::POST, target.uri("/vector_stores"), Some(&json!({"name": name})))?;
        let (status, v) = self.rag_call(req).await?;
        if !(200..300).contains(&status) {
            return Err(Self::rag_err(status, &v));
        }
        v.get("id")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| RagError { status: Some(status), message: "vector store id missing".into(), transient: false })
    }

    /// Add a file to a vector store; returns the reported indexing status.
    ///
    /// # Errors
    /// Provider or gateway failure.
    pub async fn add_vector_store_file(
        &self,
        target: &RagTarget,
        vector_store_id: &str,
        file_id: &str,
        attachment_id: Uuid,
    ) -> Result<Option<String>, RagError> {
        let body = json!({"file_id": file_id, "attributes": {"attachment_id": attachment_id.to_string()}});
        let req = Self::json_request(
            http::Method::POST,
            target.uri(&format!("/vector_stores/{vector_store_id}/files")),
            Some(&body),
        )?;
        let (status, v) = self.rag_call(req).await?;
        if !(200..300).contains(&status) {
            return Err(Self::rag_err(status, &v));
        }
        Ok(v.get("status").and_then(Value::as_str).map(str::to_owned))
    }

    /// Read the indexing status of a vector store file.
    ///
    /// # Errors
    /// Provider or gateway failure (`transient` marks retryable reads).
    pub async fn vector_store_file_status(
        &self,
        target: &RagTarget,
        vector_store_id: &str,
        file_id: &str,
    ) -> Result<Option<String>, RagError> {
        let req = Self::json_request(
            http::Method::GET,
            target.uri(&format!("/vector_stores/{vector_store_id}/files/{file_id}")),
            None,
        )?;
        let (status, v) = self.rag_call(req).await?;
        if !(200..300).contains(&status) {
            return Err(Self::rag_err(status, &v));
        }
        Ok(v.get("status").and_then(Value::as_str).map(str::to_owned))
    }

    /// Delete a vector store; `Ok(false)` when already gone.
    ///
    /// # Errors
    /// Any other non-2xx status or gateway failure.
    pub async fn delete_vector_store(&self, target: &RagTarget, vector_store_id: &str) -> Result<bool, RagError> {
        let req = Self::json_request(http::Method::DELETE, target.uri(&format!("/vector_stores/{vector_store_id}")), None)?;
        let (status, v) = self.rag_call(req).await?;
        match status {
            200..=299 => Ok(true),
            404 => Ok(false),
            _ => Err(Self::rag_err(status, &v)),
        }
    }
}

struct StreamState {
    body: oagw_sdk::body::BodyStream,
    decoder: SseDecoder,
    translator: EventTranslator,
    pending: VecDeque<ProviderEvent>,
    done: bool,
}

/// Translate a provider SSE body into [`ProviderEvent`]s as bytes arrive.
fn translate_stream(kind: ProviderKind, body: Body) -> ProviderStream {
    let state = StreamState {
        body: body.into_stream(),
        decoder: SseDecoder::new(),
        translator: EventTranslator::new(kind),
        pending: VecDeque::new(),
        done: false,
    };
    Box::pin(futures::stream::unfold(state, |mut st| async move {
        loop {
            if let Some(ev) = st.pending.pop_front() {
                return Some((ev, st));
            }
            if st.done {
                return None;
            }
            match st.body.next().await {
                Some(Ok(chunk)) => {
                    for frame in st.decoder.push(&chunk) {
                        st.pending.extend(st.translator.translate(&frame));
                    }
                }
                Some(Err(e)) => {
                    st.done = true;
                    let msg = e.to_string();
                    let code = if msg.to_lowercase().contains("timeout") || msg.to_lowercase().contains("timed out") {
                        StreamErrorCode::ProviderTimeout
                    } else {
                        StreamErrorCode::ProviderError
                    };
                    st.pending.push_back(ProviderEvent::Failed {
                        code,
                        message: "Provider stream failed".to_owned(),
                        usage: None,
                    });
                }
                None => {
                    st.done = true;
                    if let Some(frame) = st.decoder.finish() {
                        st.pending.extend(st.translator.translate(&frame));
                    }
                    st.pending.extend(st.translator.finish());
                }
            }
        }
    }))
}
