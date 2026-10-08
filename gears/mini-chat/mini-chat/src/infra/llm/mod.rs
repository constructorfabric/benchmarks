//! `llm_provider`: in-process provider adapters, OAGW provisioning, file and
//! vector-store dispatch (ADR-0001, ADR-0005).

use std::pin::Pin;
use std::sync::{Arc, OnceLock};

use futures::Stream;
use mini_chat_sdk::{ModelApiParams, UsageTokens};
use oagw_sdk::api::{ErrorSource, ServiceGatewayClientV1};
use oagw_sdk::body::Body;
use serde_json::Value;
use tokio::sync::RwLock;
use toolkit_canonical_errors::CanonicalError;
use toolkit_security::SecurityContext;

use crate::config::ProviderKind;

pub mod anthropic;
pub mod chat_completions;
pub mod openai_responses;
pub mod provision;
pub mod registry;
pub mod storage;

pub use registry::{ProviderRegistry, ResolvedProvider, ResolvedStorage};

use crate::domain::sanitize::sanitize_provider_message;

// ---------------------------------------------------------------------------
// Adapter-neutral request
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct ChatMessage {
    pub role: &'static str,
    pub text: String,
    /// Provider file ids of image inputs (`input_image`).
    pub image_file_ids: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub struct ToolsSpec {
    pub file_search: Option<(String, u32)>,
    pub web_search: Option<String>,
    pub code_interpreter: Option<Vec<String>>,
    /// `search_knowledge` function tool.
    pub knowledge_search: bool,
    pub web_search_max_uses: u32,
}

impl ToolsSpec {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.file_search.is_none()
            && self.web_search.is_none()
            && self.code_interpreter.is_none()
            && !self.knowledge_search
    }
}

#[derive(Debug, Clone)]
pub struct RequestMetadata {
    pub tenant_id: String,
    pub user_id: String,
    pub chat_id: String,
    pub request_type: &'static str,
    pub feature: String,
}

#[derive(Debug, Clone)]
pub struct ChatRequest {
    pub model: String,
    pub instructions: String,
    pub messages: Vec<ChatMessage>,
    pub max_output_tokens: i64,
    pub tools: ToolsSpec,
    pub max_tool_calls: u32,
    pub api_params: ModelApiParams,
    pub user: String,
    pub metadata: RequestMetadata,
    pub stream: bool,
    /// Extra input items appended after the messages (agentic loop).
    pub extra_input: Vec<Value>,
}

/// `user` field: `{tenant_hex}{user_hex}` (64 chars).
#[must_use]
pub fn provider_user_field(tenant: uuid::Uuid, user: uuid::Uuid) -> String {
    format!("{}{}", tenant.as_simple(), user.as_simple())
}

/// Keys the request controls; `extra_body` cannot override them.
pub const RESERVED_BODY_KEYS: &[&str] = &[
    "model",
    "input",
    "messages",
    "instructions",
    "system",
    "stream",
    "stream_options",
    "max_output_tokens",
    "max_completion_tokens",
    "max_tokens",
    "max_tool_calls",
    "tools",
    "tool_choice",
    "include",
    "store",
    "previous_response_id",
    "user",
    "metadata",
];

/// Merge `extra_body` keys into the top level of a request body.
pub fn merge_extra_body(body: &mut serde_json::Map<String, Value>, params: &ModelApiParams) {
    if let Some(extra) = &params.extra_body {
        for (k, v) in extra {
            if RESERVED_BODY_KEYS.contains(&k.as_str()) {
                tracing::warn!(key = %k, "extra_body key controlled by the request is ignored");
                continue;
            }
            body.insert(k.clone(), v.clone());
        }
    }
}

/// Insert sampling parameters that are set.
pub fn insert_sampling(body: &mut serde_json::Map<String, Value>, params: &ModelApiParams) {
    for (k, v) in [
        ("temperature", params.temperature),
        ("top_p", params.top_p),
        ("frequency_penalty", params.frequency_penalty),
        ("presence_penalty", params.presence_penalty),
    ] {
        if let Some(v) = v {
            body.insert(k.to_owned(), Value::from(v));
        }
    }
}

// ---------------------------------------------------------------------------
// Internal events
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub enum CitationSource {
    File {
        file_id: String,
        filename: Option<String>,
    },
    Web {
        url: String,
        title: String,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct RawCitation {
    pub source: CitationSource,
    pub snippet: String,
    pub span: Option<(u64, u64)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderErrorKind {
    ProviderError,
    ProviderTimeout,
    RateLimited,
}

impl ProviderErrorKind {
    #[must_use]
    pub fn code(self) -> &'static str {
        match self {
            Self::ProviderError => "provider_error",
            Self::ProviderTimeout => "provider_timeout",
            Self::RateLimited => "rate_limited",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ProviderError {
    pub kind: ProviderErrorKind,
    /// Sanitized, client-safe message.
    pub message: String,
    /// Provider error code (internal).
    pub provider_code: Option<String>,
    pub usage: Option<UsageTokens>,
}

impl ProviderError {
    #[must_use]
    pub fn new(kind: ProviderErrorKind, message: &str) -> Self {
        Self {
            kind,
            message: sanitize_provider_message(message),
            provider_code: None,
            usage: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum LlmEvent {
    TextDelta(String),
    ReasoningDelta(String),
    ToolStart {
        name: String,
        details: Value,
    },
    ToolDone {
        name: String,
        details: Value,
    },
    FunctionCall {
        call_id: String,
        name: String,
        arguments: String,
    },
    Completed {
        usage: Option<UsageTokens>,
        response_id: Option<String>,
        citations: Vec<RawCitation>,
    },
    Incomplete {
        usage: Option<UsageTokens>,
        response_id: Option<String>,
        reason: String,
    },
    Failed(ProviderError),
}

impl LlmEvent {
    #[must_use]
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            Self::Completed { .. } | Self::Incomplete { .. } | Self::Failed(_)
        )
    }
}

pub type LlmEventStream = Pin<Box<dyn Stream<Item = LlmEvent> + Send>>;

/// Result of a non-streaming completion (thread summary).
#[derive(Debug, Clone)]
pub struct Completion {
    pub text: String,
    pub usage: Option<UsageTokens>,
}

// ---------------------------------------------------------------------------
// Usage parsing helpers
// ---------------------------------------------------------------------------

fn as_i64(v: Option<&Value>) -> i64 {
    v.and_then(Value::as_i64).unwrap_or(0)
}

/// Parse a Responses / Chat Completions / Anthropic usage object.
#[must_use]
pub fn parse_usage(v: &Value) -> Option<UsageTokens> {
    if !v.is_object() {
        return None;
    }
    let input = v
        .get("input_tokens")
        .or_else(|| v.get("prompt_tokens"))
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let output = v
        .get("output_tokens")
        .or_else(|| v.get("completion_tokens"))
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let cache_read = v
        .pointer("/input_tokens_details/cached_tokens")
        .or_else(|| v.pointer("/prompt_tokens_details/cached_tokens"))
        .or_else(|| v.get("cache_read_input_tokens"))
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let reasoning = v
        .pointer("/output_tokens_details/reasoning_tokens")
        .or_else(|| v.pointer("/completion_tokens_details/reasoning_tokens"))
        .and_then(Value::as_i64)
        .unwrap_or(0);
    Some(UsageTokens {
        input_tokens: input,
        output_tokens: output,
        cache_read_input_tokens: cache_read,
        cache_write_input_tokens: as_i64(v.get("cache_creation_input_tokens")),
        reasoning_tokens: reasoning,
    })
}

// ---------------------------------------------------------------------------
// Error mapping for non-2xx proxy responses
// ---------------------------------------------------------------------------

fn extract_error_message(body: &[u8]) -> (Option<String>, String) {
    let text = String::from_utf8_lossy(body).to_string();
    if let Ok(v) = serde_json::from_slice::<Value>(body) {
        if let Some(err) = v.get("error") {
            if let Some(s) = err.as_str() {
                return (None, s.to_owned());
            }
            let msg = err
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("provider error")
                .to_owned();
            let code = err.get("code").or_else(|| err.get("type")).and_then(|c| {
                c.as_str()
                    .map(str::to_owned)
                    .or_else(|| Some(c.to_string()))
            });
            return (code, msg);
        }
        if let Some(d) = v.get("detail").and_then(Value::as_str) {
            return (None, d.to_owned());
        }
        if let Some(m) = v.get("message").and_then(Value::as_str) {
            return (
                v.get("code").and_then(Value::as_str).map(str::to_owned),
                m.to_owned(),
            );
        }
    }
    if text.trim().is_empty() {
        (None, "provider error".to_owned())
    } else {
        (None, text.chars().take(500).collect())
    }
}

/// Map a non-2xx proxy response to a provider error.
pub async fn error_from_response(resp: http::Response<Body>) -> ProviderError {
    let status = resp.status();
    let source = resp.extensions().get::<ErrorSource>().copied();
    let retry_after = resp
        .headers()
        .get(http::header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.trim().parse::<u64>().ok());
    let is_problem = resp
        .headers()
        .get(http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|ct| ct.contains("problem+json"));
    let body = resp.into_body().into_bytes().await.unwrap_or_default();
    let (code, msg) = extract_error_message(&body);
    if status == http::StatusCode::TOO_MANY_REQUESTS {
        let mut e = ProviderError::new(
            ProviderErrorKind::RateLimited,
            &match retry_after {
                Some(s) => format!("Provider rate limit reached, retry in {s}s"),
                None => "Provider rate limit reached".to_owned(),
            },
        );
        e.provider_code = code;
        return e;
    }
    let gateway_timeout = status == http::StatusCode::GATEWAY_TIMEOUT
        && (source == Some(ErrorSource::Gateway)
            || (is_problem && String::from_utf8_lossy(&body).contains("deadline_exceeded")));
    if gateway_timeout {
        return ProviderError::new(
            ProviderErrorKind::ProviderTimeout,
            "Provider request timed out",
        );
    }
    let mut e = ProviderError::new(ProviderErrorKind::ProviderError, &msg);
    e.provider_code = code;
    e
}

/// Map a proxy transport error.
#[must_use]
pub fn error_from_canonical(err: &CanonicalError) -> ProviderError {
    match err {
        CanonicalError::DeadlineExceeded { .. } => ProviderError::new(
            ProviderErrorKind::ProviderTimeout,
            "Provider request timed out",
        ),
        CanonicalError::ResourceExhausted { .. } => ProviderError::new(
            ProviderErrorKind::RateLimited,
            "Provider rate limit reached",
        ),
        other => {
            let d = other.detail().to_owned();
            if d.to_ascii_lowercase().contains("timed out")
                || d.to_ascii_lowercase().contains("timeout")
            {
                ProviderError::new(
                    ProviderErrorKind::ProviderTimeout,
                    "Provider request timed out",
                )
            } else {
                ProviderError::new(
                    ProviderErrorKind::ProviderError,
                    "Provider is currently unavailable",
                )
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Gateway
// ---------------------------------------------------------------------------

/// OAGW access with the S2S security context.
pub struct LlmGateway {
    pub registry: ProviderRegistry,
    gw: OnceLock<Arc<dyn ServiceGatewayClientV1>>,
    s2s: RwLock<Option<SecurityContext>>,
}

impl LlmGateway {
    #[must_use]
    pub fn new(registry: ProviderRegistry) -> Self {
        Self {
            registry,
            gw: OnceLock::new(),
            s2s: RwLock::new(None),
        }
    }

    pub fn set_gateway(&self, gw: Arc<dyn ServiceGatewayClientV1>) {
        if self.gw.set(gw).is_err() {
            tracing::debug!("OAGW client already set");
        }
    }

    pub async fn set_s2s(&self, ctx: SecurityContext) {
        *self.s2s.write().await = Some(ctx);
    }

    pub async fn s2s(&self) -> Option<SecurityContext> {
        self.s2s.read().await.clone()
    }

    /// # Errors
    /// When the OAGW client is not resolved.
    pub fn gateway(&self) -> Result<Arc<dyn ServiceGatewayClientV1>, ProviderError> {
        self.gw.get().cloned().ok_or_else(|| {
            ProviderError::new(
                ProviderErrorKind::ProviderError,
                "Provider gateway unavailable",
            )
        })
    }

    /// Proxy a request through OAGW, using the S2S context when available
    /// (upstreams are provisioned under it) and the caller's otherwise.
    ///
    /// # Errors
    /// Transport errors are mapped to provider errors.
    pub async fn proxy(
        &self,
        fallback_ctx: &SecurityContext,
        req: http::Request<Body>,
    ) -> Result<http::Response<Body>, ProviderError> {
        let gw = self.gateway()?;
        let ctx = self.s2s().await.unwrap_or_else(|| fallback_ctx.clone());
        gw.proxy_request(ctx, req).await.map_err(|e| {
            tracing::warn!(error = %e, "OAGW proxy request failed");
            error_from_canonical(&e)
        })
    }

    /// Start a streaming chat request.
    ///
    /// # Errors
    /// Returns the domain error of the step that failed (validation,
    /// authorization, persistence or a downstream dependency).
    pub async fn stream_chat(
        &self,
        ctx: &SecurityContext,
        provider: &ResolvedProvider,
        req: &ChatRequest,
    ) -> Result<LlmEventStream, ProviderError> {
        match provider.kind {
            ProviderKind::OpenaiResponses | ProviderKind::VllmResponses => {
                openai_responses::stream(self, ctx, provider, req).await
            }
            ProviderKind::OpenaiChatCompletions => {
                chat_completions::stream(self, ctx, provider, req).await
            }
            ProviderKind::AnthropicMessages => anthropic::stream(self, ctx, provider, req).await,
        }
    }

    /// Non-streaming completion (thread summary).
    ///
    /// # Errors
    /// Returns the domain error of the step that failed (validation,
    /// authorization, persistence or a downstream dependency).
    pub async fn complete(
        &self,
        ctx: &SecurityContext,
        provider: &ResolvedProvider,
        req: &ChatRequest,
    ) -> Result<Completion, ProviderError> {
        match provider.kind {
            ProviderKind::OpenaiResponses | ProviderKind::VllmResponses => {
                openai_responses::complete(self, ctx, provider, req).await
            }
            ProviderKind::OpenaiChatCompletions => {
                chat_completions::complete(self, ctx, provider, req).await
            }
            ProviderKind::AnthropicMessages => anthropic::complete(self, ctx, provider, req).await,
        }
    }
}

/// Build the chat URL `/{alias}{api_path}` with `{model}` replaced.
#[must_use]
pub fn chat_url(provider: &ResolvedProvider, model: &str) -> String {
    let path = provider.api_path.replace("{model}", model);
    format!("/{}{}", provider.alias, path)
}

/// Build a JSON POST request.
///
/// # Errors
/// Request construction failure.
pub fn json_post(url: &str, body: &Value) -> Result<http::Request<Body>, ProviderError> {
    let bytes = serde_json::to_vec(body)
        .map_err(|e| ProviderError::new(ProviderErrorKind::ProviderError, &e.to_string()))?;
    http::Request::builder()
        .method(http::Method::POST)
        .uri(url)
        .header(http::header::CONTENT_TYPE, "application/json")
        .header(http::header::ACCEPT, "text/event-stream, application/json")
        .body(Body::from(bytes))
        .map_err(|e| ProviderError::new(ProviderErrorKind::ProviderError, &e.to_string()))
}

/// Classify a mid-stream body read error.
#[must_use]
pub fn stream_read_error(e: &str) -> ProviderError {
    let l = e.to_ascii_lowercase();
    if l.contains("timeout") || l.contains("timed out") || l.contains("deadline") {
        ProviderError::new(
            ProviderErrorKind::ProviderTimeout,
            "Provider request timed out",
        )
    } else {
        ProviderError::new(ProviderErrorKind::ProviderError, "Provider stream failed")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_field_is_64_chars() {
        let s = provider_user_field(uuid::Uuid::from_u128(1), uuid::Uuid::from_u128(2));
        assert_eq!(s.len(), 64);
        assert!(!s.contains('-'));
    }

    #[test]
    fn usage_parsing() {
        let u = parse_usage(&serde_json::json!({
            "input_tokens": 10, "output_tokens": 5,
            "input_tokens_details": {"cached_tokens": 3},
            "output_tokens_details": {"reasoning_tokens": 2}
        }))
        .unwrap();
        assert_eq!(
            (
                u.input_tokens,
                u.output_tokens,
                u.cache_read_input_tokens,
                u.reasoning_tokens
            ),
            (10, 5, 3, 2)
        );
        let c =
            parse_usage(&serde_json::json!({"prompt_tokens": 7, "completion_tokens": 1})).unwrap();
        assert_eq!((c.input_tokens, c.output_tokens), (7, 1));
        assert!(parse_usage(&Value::Null).is_none());
    }

    #[test]
    fn extra_body_cannot_override_reserved_keys() {
        let mut body = serde_json::Map::new();
        body.insert("model".into(), "m".into());
        let mut extra = serde_json::Map::new();
        extra.insert("model".into(), "evil".into());
        extra.insert("seed".into(), 7.into());
        let params = ModelApiParams {
            extra_body: Some(extra),
            ..ModelApiParams::default()
        };
        merge_extra_body(&mut body, &params);
        assert_eq!(body["model"], "m");
        assert_eq!(body["seed"], 7);
    }

    #[test]
    fn error_message_extraction() {
        let (c, m) =
            extract_error_message(br#"{"error":{"message":"bad resp_abc123","code":"x"}}"#);
        assert_eq!(c.as_deref(), Some("x"));
        assert_eq!(m, "bad resp_abc123");
        let e = ProviderError::new(ProviderErrorKind::ProviderError, &m);
        assert_eq!(e.message, "bad [provider_id]");
    }
}
