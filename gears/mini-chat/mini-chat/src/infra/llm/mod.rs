//! `llm_provider` library (ADR-0001, ADR-0005): provider resolution, adapters,
//! OAGW transport, file/vector-store storage and OAGW provisioning.

pub mod openai_responses;
pub mod other_adapters;
pub mod provisioning;
pub mod sse;
pub mod storage;
pub mod types;

use std::collections::HashMap;
use std::pin::Pin;
use std::sync::Arc;

use bytes::Bytes;
use futures::{Stream, StreamExt};
use oagw_sdk::{Body, ServiceGatewayClientV1};
use parking_lot::RwLock;
use serde_json::Value;
use toolkit_canonical_errors::CanonicalError;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::config::{ProviderConfig, ProviderKind, StorageKind};
use sse::SseParser;
use types::{LlmEvent, LlmRequest, ProviderErrorKind, ProviderFailure};

/// Stream of internal events of one provider call.
pub type LlmEventStream = Pin<Box<dyn Stream<Item = LlmEvent> + Send>>;

/// Holder of the S2S security context obtained at gear start.
#[derive(Default)]
pub struct S2sContext {
    ctx: RwLock<Option<SecurityContext>>,
}

impl S2sContext {
    pub fn set(&self, ctx: SecurityContext) {
        *self.ctx.write() = Some(ctx);
    }

    #[must_use]
    pub fn get(&self) -> Option<SecurityContext> {
        self.ctx.read().clone()
    }

    /// Context used for proxying: S2S when available, else the caller's.
    #[must_use]
    pub fn proxy_ctx(&self, fallback: &SecurityContext) -> SecurityContext {
        self.get().unwrap_or_else(|| fallback.clone())
    }
}

/// Resolved chat endpoint of a provider for a tenant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderTarget {
    pub provider_id: String,
    pub kind: ProviderKind,
    pub alias: String,
    pub api_path: String,
}

/// Resolved storage (files / vector stores) endpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StorageTarget {
    /// Provider entry that serves the storage API.
    pub provider_id: String,
    pub storage_kind: StorageKind,
    pub alias: String,
    pub api_version: Option<String>,
    /// Label stored in `attachments.storage_backend` / `chat_vector_stores.provider`.
    pub backend_label: String,
}

impl StorageTarget {
    /// Path prefix of the storage API (`/v1` or `/openai`).
    #[must_use]
    pub fn prefix(&self) -> &'static str {
        match self.storage_kind {
            StorageKind::Openai => "/v1",
            StorageKind::Azure => "/openai",
        }
    }

    /// Query string (`?api-version=...` for Azure).
    #[must_use]
    pub fn query(&self) -> String {
        match (&self.storage_kind, &self.api_version) {
            (StorageKind::Azure, Some(v)) => format!("?api-version={v}"),
            _ => String::new(),
        }
    }
}

/// Alias under which an upstream is registered: the configured alias, else
/// the host (`host:port` for non-standard ports, as OAGW derives it).
#[must_use]
pub fn configured_alias(upstream_alias: Option<&String>, host: &str, port: u16) -> String {
    upstream_alias
        .filter(|a| !a.trim().is_empty())
        .cloned()
        .unwrap_or_else(|| provisioning::default_alias(host, port))
}

/// On-demand provisioning of an upstream that is still pending.
#[async_trait::async_trait]
pub trait ProvisionHook: Send + Sync {
    /// Attempts to provision the upstream with this alias now.
    async fn ensure_ready(&self, alias: &str);
}

/// Bound of an on-demand provisioning attempt on the request path.
const ON_DEMAND_PROVISION_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);

/// Maps provider entries (and tenant overrides) to OAGW aliases.
pub struct ProviderResolver {
    providers: HashMap<String, ProviderConfig>,
    /// Aliases reported by OAGW when they differ from the configured ones.
    actual: RwLock<HashMap<String, String>>,
    /// Configured aliases whose upstream is not provisioned yet.
    pending: RwLock<std::collections::HashSet<String>>,
    hook: std::sync::OnceLock<Arc<dyn ProvisionHook>>,
}

impl ProviderResolver {
    #[must_use]
    pub fn new(providers: HashMap<String, ProviderConfig>) -> Self {
        Self {
            providers,
            actual: RwLock::new(HashMap::new()),
            pending: RwLock::new(std::collections::HashSet::new()),
            hook: std::sync::OnceLock::new(),
        }
    }

    /// Installs the on-demand provisioning hook.
    pub fn set_hook(&self, hook: Arc<dyn ProvisionHook>) {
        self.hook.set(hook).ok();
    }

    /// Marks upstream aliases as pending (deferred at start).
    pub fn set_pending(&self, aliases: impl IntoIterator<Item = String>) {
        let mut p = self.pending.write();
        p.clear();
        p.extend(aliases);
    }

    /// Marks an upstream alias as provisioned.
    pub fn mark_ready(&self, alias: &str) {
        self.pending.write().remove(alias);
    }

    /// Whether an alias is still waiting for provisioning.
    #[must_use]
    pub fn is_pending(&self, alias: &str) -> bool {
        self.pending.read().contains(alias)
    }

    /// Before a proxied call: provisions a pending upstream right away
    /// (bounded), so a credential that became readable is used without
    /// waiting for the next reconcile round.
    pub async fn ensure_ready(&self, alias: &str) {
        if !self.is_pending(alias) {
            return;
        }
        if let Some(h) = self.hook.get() {
            tokio::time::timeout(ON_DEMAND_PROVISION_TIMEOUT, h.ensure_ready(alias)).await.ok();
        }
    }

    /// Provider entries.
    #[must_use]
    pub fn providers(&self) -> &HashMap<String, ProviderConfig> {
        &self.providers
    }

    /// Records the alias OAGW actually assigned for a configured alias.
    pub fn record_actual_alias(&self, configured: &str, actual: &str) {
        if configured != actual {
            self.actual
                .write()
                .insert(configured.to_owned(), actual.to_owned());
        }
    }

    fn effective_alias(&self, configured: String) -> String {
        self.actual.read().get(&configured).cloned().unwrap_or(configured)
    }

    fn alias_for(&self, p: &ProviderConfig, tenant_id: Uuid) -> String {
        let key = tenant_id.to_string();
        if let Some(o) = p.tenant_overrides.get(&key) {
            let host = o.host.as_deref().unwrap_or(&p.host);
            return self.effective_alias(configured_alias(
                o.upstream_alias.as_ref(),
                host,
                p.effective_port(),
            ));
        }
        self.effective_alias(configured_alias(
            p.upstream_alias.as_ref(),
            &p.host,
            p.effective_port(),
        ))
    }

    /// Chat target of a provider for a tenant.
    #[must_use]
    pub fn resolve(&self, provider_id: &str, tenant_id: Uuid) -> Option<ProviderTarget> {
        let p = self.providers.get(provider_id)?;
        Some(ProviderTarget {
            provider_id: provider_id.to_owned(),
            kind: p.kind,
            alias: self.alias_for(p, tenant_id),
            api_path: p.api_path.clone(),
        })
    }

    /// Storage target of a provider (follows `rag_provider`).
    #[must_use]
    pub fn storage_for(&self, provider_id: &str, tenant_id: Uuid) -> Option<StorageTarget> {
        let p = self.providers.get(provider_id)?;
        let (sid, sp) = match &p.rag_provider {
            Some(rag) => (rag.as_str(), self.providers.get(rag)?),
            None => (provider_id, p),
        };
        let kind = sp.storage_kind?;
        Some(StorageTarget {
            provider_id: sid.to_owned(),
            storage_kind: kind,
            alias: self.alias_for(sp, tenant_id),
            api_version: sp.api_version.clone(),
            backend_label: sp.storage_backend.clone().unwrap_or_else(|| sid.to_owned()),
        })
    }

    /// Storage target by backend label (cleanup path).
    #[must_use]
    pub fn storage_by_label(&self, label: &str, tenant_id: Uuid) -> Option<StorageTarget> {
        let id = self
            .providers
            .iter()
            .find(|(id, p)| p.storage_backend.as_deref() == Some(label) || (p.storage_backend.is_none() && id.as_str() == label))
            .map(|(id, _)| id.clone())?;
        self.storage_for(&id, tenant_id)
    }
}

/// Non-streaming completion result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Completion {
    pub text: String,
    pub usage: Option<mini_chat_sdk::UsageTokens>,
}

/// Provider client over the OAGW in-process proxy.
pub struct LlmClient {
    gateway: Arc<dyn ServiceGatewayClientV1>,
    resolver: Arc<ProviderResolver>,
    s2s: Arc<S2sContext>,
}

fn map_gateway_err(e: &CanonicalError) -> ProviderFailure {
    let kind = match e {
        CanonicalError::DeadlineExceeded { .. } => ProviderErrorKind::ProviderTimeout,
        CanonicalError::ResourceExhausted { .. } => ProviderErrorKind::RateLimited,
        _ => ProviderErrorKind::ProviderError,
    };
    tracing::warn!(error = %e, "provider request failed at the gateway");
    ProviderFailure {
        kind,
        message: match kind {
            ProviderErrorKind::ProviderTimeout => "Provider request timed out".to_owned(),
            ProviderErrorKind::RateLimited => "Provider rate limit exceeded".to_owned(),
            ProviderErrorKind::ProviderError => "Provider is currently unavailable".to_owned(),
        },
        usage: None,
        response_id: None,
    }
}

async fn read_body(body: Body) -> Bytes {
    body.into_bytes().await.unwrap_or_default()
}

/// Maps a non-2xx provider response to a failure.
async fn http_failure(resp: http::Response<Body>) -> ProviderFailure {
    let status = resp.status();
    let from_gateway = matches!(
        resp.extensions().get::<oagw_sdk::api::ErrorSource>(),
        Some(oagw_sdk::api::ErrorSource::Gateway)
    );
    let retry_after = resp
        .headers()
        .get(http::header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<u64>().ok());
    let body = read_body(resp.into_body()).await;
    let parsed: Option<Value> = serde_json::from_slice(&body).ok();
    if status.as_u16() == 429 {
        let mut message = "Provider rate limit exceeded".to_owned();
        if let Some(n) = retry_after {
            message = format!("{message}, retry in {n}s");
        }
        return ProviderFailure {
            kind: ProviderErrorKind::RateLimited,
            message,
            usage: None,
            response_id: None,
        };
    }
    if from_gateway && status.as_u16() == 504 {
        return ProviderFailure {
            kind: ProviderErrorKind::ProviderTimeout,
            message: "Provider request timed out".to_owned(),
            usage: None,
            response_id: None,
        };
    }
    let message = parsed
        .as_ref()
        .filter(|_| !from_gateway)
        .and_then(openai_responses::error_message)
        .unwrap_or_else(|| format!("Provider returned HTTP {}", status.as_u16()));
    ProviderFailure::error(message)
}

impl LlmClient {
    #[must_use]
    pub fn new(
        gateway: Arc<dyn ServiceGatewayClientV1>,
        resolver: Arc<ProviderResolver>,
        s2s: Arc<S2sContext>,
    ) -> Self {
        Self {
            gateway,
            resolver,
            s2s,
        }
    }

    #[must_use]
    pub fn resolver(&self) -> &Arc<ProviderResolver> {
        &self.resolver
    }

    #[must_use]
    pub fn gateway(&self) -> &Arc<dyn ServiceGatewayClientV1> {
        &self.gateway
    }

    #[must_use]
    pub fn s2s(&self) -> &Arc<S2sContext> {
        &self.s2s
    }

    fn body_for(target: &ProviderTarget, req: &LlmRequest) -> Value {
        match target.kind {
            ProviderKind::OpenaiResponses => openai_responses::build_request(req, true, true),
            ProviderKind::VllmResponses => openai_responses::build_request(req, false, false),
            ProviderKind::OpenaiChatCompletions => other_adapters::build_chat_completions(req),
            ProviderKind::AnthropicMessages => other_adapters::build_anthropic(req),
        }
    }

    fn uri_for(target: &ProviderTarget, model: &str) -> String {
        let path = target.api_path.replace("{model}", model);
        format!("/{}{}", target.alias, path)
    }

    async fn send(
        &self,
        target: &ProviderTarget,
        req: &LlmRequest,
        ctx: &SecurityContext,
    ) -> Result<http::Response<Body>, ProviderFailure> {
        let body = Self::body_for(target, req);
        let bytes = serde_json::to_vec(&body).map_err(|e| ProviderFailure::error(e.to_string()))?;
        let mut builder = http::Request::builder()
            .method(http::Method::POST)
            .uri(Self::uri_for(target, &req.model))
            .header(http::header::CONTENT_TYPE, "application/json");
        if req.stream {
            builder = builder.header(http::header::ACCEPT, "text/event-stream");
        }
        let http_req = builder
            .body(Body::from(bytes))
            .map_err(|e| ProviderFailure::error(e.to_string()))?;
        self.resolver.ensure_ready(&target.alias).await;
        let resp = self
            .gateway
            .proxy_request(self.s2s.proxy_ctx(ctx), http_req)
            .await
            .map_err(|e| map_gateway_err(&e))?;
        if !resp.status().is_success() {
            return Err(http_failure(resp).await);
        }
        Ok(resp)
    }

    /// Starts a streaming chat call.
    ///
    /// # Errors
    /// Provider failure before the stream started.
    pub async fn stream_chat(
        &self,
        target: &ProviderTarget,
        req: &LlmRequest,
        ctx: &SecurityContext,
    ) -> Result<LlmEventStream, ProviderFailure> {
        let resp = self.send(target, req, ctx).await?;
        let kind = target.kind;
        let mut body = resp.into_body().into_stream();
        let stream = async_stream::stream! {
            let mut parser = SseParser::new();
            let mut responses = openai_responses::ResponsesTranslator::new();
            let mut chat = other_adapters::ChatCompletionsTranslator::new();
            let mut anthropic = other_adapters::AnthropicTranslator::new();
            let mut terminal = false;
            loop {
                let chunk = body.next().await;
                let eof = chunk.is_none();
                let frames = match chunk {
                    Some(Ok(bytes)) => parser.feed(&bytes),
                    Some(Err(e)) => {
                        tracing::warn!(error = %e, "provider stream failed");
                        if !terminal {
                            yield LlmEvent::Failed(ProviderFailure::error("Provider stream failed"));
                        }
                        return;
                    }
                    None => parser.finish(),
                };
                for frame in &frames {
                    let events = match kind {
                        ProviderKind::OpenaiResponses | ProviderKind::VllmResponses => responses.on_frame(frame),
                        ProviderKind::OpenaiChatCompletions => chat.on_frame(frame),
                        ProviderKind::AnthropicMessages => anthropic.on_frame(frame),
                    };
                    for ev in events {
                        if matches!(ev, LlmEvent::Completed { .. } | LlmEvent::Failed(_)) {
                            terminal = true;
                        }
                        yield ev;
                    }
                }
                if terminal {
                    return;
                }
                if eof {
                    let tail = match kind {
                        ProviderKind::OpenaiChatCompletions => chat.finish(),
                        ProviderKind::AnthropicMessages if !anthropic.is_terminal() => {
                            vec![LlmEvent::Failed(ProviderFailure::error("Provider stream ended unexpectedly"))]
                        }
                        _ => vec![LlmEvent::Failed(ProviderFailure::error("Provider stream ended unexpectedly"))],
                    };
                    for ev in tail {
                        yield ev;
                    }
                    return;
                }
            }
        };
        Ok(Box::pin(stream))
    }

    /// Non-streaming completion (thread summary).
    ///
    /// # Errors
    /// Provider failure.
    pub async fn complete(
        &self,
        target: &ProviderTarget,
        req: &LlmRequest,
        ctx: &SecurityContext,
    ) -> Result<Completion, ProviderFailure> {
        let mut req = req.clone();
        req.stream = false;
        let resp = self.send(target, &req, ctx).await?;
        let is_sse = resp
            .headers()
            .get(http::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.starts_with("text/event-stream"));
        let body = read_body(resp.into_body()).await;
        let looks_sse = is_sse || body.starts_with(b"event:") || body.starts_with(b"data:");
        if looks_sse {
            return collect_sse_completion(target.kind, &body);
        }
        let v: Value = serde_json::from_slice(&body)
            .map_err(|_| ProviderFailure::error("Provider returned an invalid response"))?;
        Ok(parse_completion_json(target.kind, &v))
    }
}

fn collect_sse_completion(kind: ProviderKind, body: &[u8]) -> Result<Completion, ProviderFailure> {
    let mut parser = SseParser::new();
    let mut frames = parser.feed(body);
    frames.extend(parser.finish());
    let mut responses = openai_responses::ResponsesTranslator::new();
    let mut chat = other_adapters::ChatCompletionsTranslator::new();
    let mut anthropic = other_adapters::AnthropicTranslator::new();
    let mut text = String::new();
    let mut usage = None;
    let mut events = Vec::new();
    for f in &frames {
        events.extend(match kind {
            ProviderKind::OpenaiResponses | ProviderKind::VllmResponses => responses.on_frame(f),
            ProviderKind::OpenaiChatCompletions => chat.on_frame(f),
            ProviderKind::AnthropicMessages => anthropic.on_frame(f),
        });
    }
    if kind == ProviderKind::OpenaiChatCompletions {
        events.extend(chat.finish());
    }
    for ev in events {
        match ev {
            LlmEvent::TextDelta(t) => text.push_str(&t),
            LlmEvent::Completed { usage: u, .. } => usage = u,
            LlmEvent::Failed(f) => return Err(f),
            _ => {}
        }
    }
    Ok(Completion { text, usage })
}

/// Extracts text and usage from a non-streaming provider response.
#[must_use]
pub fn parse_completion_json(kind: ProviderKind, v: &Value) -> Completion {
    let text = match kind {
        ProviderKind::OpenaiChatCompletions => v
            .pointer("/choices/0/message/content")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        ProviderKind::AnthropicMessages => v
            .get("content")
            .and_then(Value::as_array)
            .map(|c| {
                c.iter()
                    .filter_map(|b| b.get("text").and_then(Value::as_str))
                    .collect::<Vec<_>>()
                    .join("")
            })
            .unwrap_or_default(),
        ProviderKind::OpenaiResponses | ProviderKind::VllmResponses => {
            if let Some(t) = v.get("output_text").and_then(Value::as_str) {
                t.to_owned()
            } else {
                v.get("output")
                    .and_then(Value::as_array)
                    .map(|items| {
                        items
                            .iter()
                            .filter_map(|i| i.get("content").and_then(Value::as_array))
                            .flatten()
                            .filter(|c| c.get("type").and_then(Value::as_str) == Some("output_text"))
                            .filter_map(|c| c.get("text").and_then(Value::as_str))
                            .collect::<Vec<_>>()
                            .join("")
                    })
                    .unwrap_or_default()
            }
        }
    };
    let usage = match kind {
        ProviderKind::AnthropicMessages => v.get("usage").map(|u| mini_chat_sdk::UsageTokens {
            input_tokens: u.get("input_tokens").and_then(Value::as_i64).unwrap_or(0),
            output_tokens: u.get("output_tokens").and_then(Value::as_i64).unwrap_or(0),
            ..mini_chat_sdk::UsageTokens::default()
        }),
        _ => v.get("usage").and_then(openai_responses::parse_usage),
    };
    Completion { text, usage }
}
