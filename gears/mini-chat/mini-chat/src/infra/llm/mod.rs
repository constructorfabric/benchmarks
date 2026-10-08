//! `llm_provider`: provider resolution, OAGW transport, adapters and storage
//! (ADR-0001, ADR-0002, ADR-0005).
//!
//! All OAGW calls run under the gear's S2S security context: upstreams are
//! registered under that context at start, and OAGW resolves aliases per tenant.

pub mod provisioning;
pub mod responses;
pub mod storage;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use authn_resolver_sdk::{AuthNResolverClient, ClientCredentialsRequest};
use bytes::Bytes;
use oagw_sdk::{Body, ServiceGatewayClientV1};
use secrecy::ExposeSecret;
use tokio::sync::RwLock;
use toolkit_canonical_errors::CanonicalError;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::config::{ClientCredentialsConfig, ProviderEntry, ProviderKind, StorageKind};
use crate::domain::error::{DomainError, DomainResult};
use crate::domain::sanitize::sanitize_provider_message;

/// A provider entry resolved for a tenant.
#[derive(Debug, Clone)]
pub struct ResolvedProvider {
    pub provider_id: String,
    pub kind: ProviderKind,
    pub alias: String,
    pub api_path: String,
    pub storage_kind: StorageKind,
    pub storage_backend: String,
    pub api_version: Option<String>,
}

impl ResolvedProvider {
    /// Proxy URI of the chat endpoint for a provider model name.
    #[must_use]
    pub fn chat_uri(&self, provider_model_id: &str) -> String {
        format!(
            "/{}{}",
            self.alias,
            self.api_path.replace("{model}", provider_model_id)
        )
    }

    /// RAG path prefix: `/v1` (openai) or `/openai` (azure).
    #[must_use]
    pub fn rag_prefix(&self) -> &'static str {
        match self.storage_kind {
            StorageKind::Openai => "/v1",
            StorageKind::Azure => "/openai",
        }
    }

    /// Proxy URI of a RAG endpoint (`path` starts with `/`).
    #[must_use]
    pub fn rag_uri(&self, path: &str) -> String {
        let base = format!("/{}{}{}", self.alias, self.rag_prefix(), path);
        match (&self.storage_kind, &self.api_version) {
            (StorageKind::Azure, Some(v)) => {
                if base.contains('?') {
                    format!("{base}&api-version={v}")
                } else {
                    format!("{base}?api-version={v}")
                }
            }
            _ => base,
        }
    }
}

/// Failure of a provider call.
#[derive(Debug, Clone)]
pub enum ProviderCallError {
    /// Gateway or provider timeout.
    Timeout(String),
    /// Provider 429.
    RateLimited {
        retry_after_secs: Option<u64>,
        message: String,
    },
    /// Any other provider or transport error; `transient` is true for 5xx and gateway failures.
    Provider {
        status: Option<u16>,
        message: String,
        transient: bool,
    },
    /// Context length exceeded (thread-summary retry trigger).
    ContextLength(String),
}

impl ProviderCallError {
    /// Streaming error code (DESIGN §3.3 "Streaming error codes").
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::Timeout(_) => "provider_timeout",
            Self::RateLimited { .. } => "rate_limited",
            Self::Provider { .. } | Self::ContextLength(_) => "provider_error",
        }
    }

    /// Sanitized client message.
    #[must_use]
    pub fn client_message(&self) -> String {
        match self {
            Self::Timeout(_) => "The provider request timed out".to_owned(),
            Self::RateLimited {
                retry_after_secs, ..
            } => match retry_after_secs {
                Some(s) => format!("The provider is rate limiting requests; retry in {s}s"),
                None => "The provider is rate limiting requests".to_owned(),
            },
            Self::Provider { message, .. } | Self::ContextLength(message) => {
                let m = sanitize_provider_message(message);
                if m.trim().is_empty() {
                    "Provider is currently unavailable".to_owned()
                } else {
                    m
                }
            }
        }
    }

    /// `true` for retryable failures (5xx, gateway errors, timeouts).
    #[must_use]
    pub fn is_transient(&self) -> bool {
        match self {
            Self::Timeout(_) | Self::RateLimited { .. } => true,
            Self::Provider { transient, .. } => *transient,
            Self::ContextLength(_) => false,
        }
    }
}

impl std::fmt::Display for ProviderCallError {
    // `{status:?}` renders `Some(..)`/`None`; the message text is kept byte-for-byte.
    #[allow(clippy::use_debug)]
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Timeout(m) => write!(f, "timeout: {m}"),
            Self::RateLimited { message, .. } => write!(f, "rate limited: {message}"),
            Self::Provider {
                status, message, ..
            } => write!(f, "provider error ({status:?}): {message}"),
            Self::ContextLength(m) => write!(f, "context length exceeded: {m}"),
        }
    }
}

/// Extracts `error.message` (or a flat `message`) from a provider error body.
#[must_use]
pub fn error_message_from_body(body: &[u8]) -> (Option<String>, String) {
    if let Ok(v) = serde_json::from_slice::<serde_json::Value>(body) {
        let err = v.get("error").unwrap_or(&v);
        let code = err.get("code").and_then(|c| {
            c.as_str()
                .map(str::to_owned)
                .or_else(|| c.as_i64().map(|n| n.to_string()))
        });
        let msg = err
            .get("message")
            .and_then(serde_json::Value::as_str)
            .or_else(|| v.get("detail").and_then(serde_json::Value::as_str))
            .or_else(|| err.as_str())
            .unwrap_or("")
            .to_owned();
        return (code, msg);
    }
    (
        None,
        String::from_utf8_lossy(body).chars().take(500).collect(),
    )
}

fn is_context_length(code: Option<&str>, msg: &str) -> bool {
    code == Some("context_length_exceeded")
        || msg.contains("context_length_exceeded")
        || msg.to_ascii_lowercase().contains("maximum context length")
}

/// Maps a gateway error (`Err` of `proxy_request`).
#[must_use]
pub fn map_gateway_error(e: &CanonicalError) -> ProviderCallError {
    let status = e.status_code();
    if status == 504 || matches!(e, CanonicalError::DeadlineExceeded { .. }) {
        return ProviderCallError::Timeout(e.to_string());
    }
    ProviderCallError::Provider {
        status: Some(status),
        message: "Provider is currently unavailable".to_owned(),
        transient: status >= 500 || status == 429,
    }
}

/// Maps a non-2xx upstream response.
pub async fn map_upstream_error(resp: http::Response<Body>) -> ProviderCallError {
    let status = resp.status().as_u16();
    let retry_after = resp
        .headers()
        .get(http::header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<u64>().ok());
    let body = resp
        .into_body()
        .into_bytes()
        .await
        .unwrap_or_else(|_| Bytes::new());
    let (code, msg) = error_message_from_body(&body);
    if status == 429 {
        return ProviderCallError::RateLimited {
            retry_after_secs: retry_after,
            message: msg,
        };
    }
    if is_context_length(code.as_deref(), &msg) {
        return ProviderCallError::ContextLength(msg);
    }
    ProviderCallError::Provider {
        status: Some(status),
        message: if msg.is_empty() {
            format!("provider returned HTTP {status}")
        } else {
            msg
        },
        transient: status >= 500,
    }
}

/// Provider resolution and OAGW transport.
pub struct LlmGateway {
    oagw: Arc<dyn ServiceGatewayClientV1>,
    authn: Arc<dyn AuthNResolverClient>,
    creds: ClientCredentialsConfig,
    s2s: RwLock<Option<SecurityContext>>,
    providers: HashMap<String, ProviderEntry>,
    /// Configured alias (lowercase) -> alias OAGW registered, when they differ.
    alias_overrides: std::sync::RwLock<HashMap<String, String>>,
}

impl LlmGateway {
    #[must_use]
    pub fn new(
        oagw: Arc<dyn ServiceGatewayClientV1>,
        authn: Arc<dyn AuthNResolverClient>,
        creds: ClientCredentialsConfig,
        providers: HashMap<String, ProviderEntry>,
    ) -> Self {
        Self {
            oagw,
            authn,
            creds,
            s2s: RwLock::new(None),
            providers,
            alias_overrides: std::sync::RwLock::new(HashMap::new()),
        }
    }

    /// Records the alias OAGW registered for a configured alias.
    pub fn set_alias_override(&self, configured: &str, actual: &str) {
        if let Ok(mut m) = self.alias_overrides.write() {
            m.insert(configured.to_ascii_lowercase(), actual.to_ascii_lowercase());
        }
    }

    #[must_use]
    pub fn oagw(&self) -> &Arc<dyn ServiceGatewayClientV1> {
        &self.oagw
    }

    #[must_use]
    pub fn providers(&self) -> &HashMap<String, ProviderEntry> {
        &self.providers
    }

    /// Exchanges the client credentials for an S2S security context.
    ///
    /// # Errors
    /// `AuthN` failures.
    pub async fn exchange_s2s(&self) -> DomainResult<SecurityContext> {
        let req = ClientCredentialsRequest {
            client_id: self.creds.client_id.clone(),
            client_secret: secrecy::SecretString::from(
                self.creds.client_secret.expose_secret().to_owned(),
            ),
            scopes: Vec::new(),
        };
        let res = self
            .authn
            .exchange_client_credentials(&req)
            .await
            .map_err(|e| {
                DomainError::internal(format!("client credentials exchange failed: {e}"))
            })?;
        let ctx = res.security_context;
        *self.s2s.write().await = Some(ctx.clone());
        Ok(ctx)
    }

    /// Cached S2S context, exchanged on first use.
    ///
    /// # Errors
    /// `AuthN` failures.
    pub async fn s2s_ctx(&self) -> DomainResult<SecurityContext> {
        if let Some(ctx) = self.s2s.read().await.clone() {
            return Ok(ctx);
        }
        self.exchange_s2s().await
    }

    /// Resolves a provider entry (and the tenant override) for a tenant.
    ///
    /// # Errors
    /// `Internal` for an unknown provider id.
    pub fn resolve(&self, provider_id: &str, tenant_id: Uuid) -> DomainResult<ResolvedProvider> {
        let p = self
            .providers
            .get(provider_id)
            .ok_or_else(|| DomainError::internal(format!("unknown provider '{provider_id}'")))?;
        let alias = if let Some(o) = p.tenant_overrides.get(&tenant_id.to_string())
            && let Some(a) = o
                .upstream_alias
                .clone()
                .or_else(|| o.host.as_deref().map(|h| p.default_alias_for(h)))
        {
            a
        } else {
            p.upstream_alias
                .clone()
                .unwrap_or_else(|| p.default_alias_for(&p.host))
        };
        let alias = alias.to_ascii_lowercase();
        let alias = self
            .alias_overrides
            .read()
            .ok()
            .and_then(|m| m.get(&alias).cloned())
            .unwrap_or(alias);
        Ok(ResolvedProvider {
            provider_id: provider_id.to_owned(),
            kind: p.kind,
            alias: alias.to_ascii_lowercase(),
            api_path: p.api_path.clone(),
            storage_kind: p.storage_kind,
            storage_backend: p
                .storage_backend
                .clone()
                .unwrap_or_else(|| provider_id.to_owned()),
            api_version: p.api_version.clone(),
        })
    }

    /// The provider used for file and vector-store operations (`rag_provider` or itself).
    ///
    /// # Errors
    /// Unknown provider ids.
    pub fn resolve_rag(
        &self,
        provider_id: &str,
        tenant_id: Uuid,
    ) -> DomainResult<ResolvedProvider> {
        let p = self
            .providers
            .get(provider_id)
            .ok_or_else(|| DomainError::internal(format!("unknown provider '{provider_id}'")))?;
        match p.rag_provider.as_deref() {
            Some(rag) => self.resolve(rag, tenant_id),
            None => self.resolve(provider_id, tenant_id),
        }
    }

    /// Finds the provider whose storage backend label is `backend`.
    #[must_use]
    pub fn provider_for_backend(&self, backend: &str) -> Option<String> {
        self.providers
            .iter()
            .find(|(id, p)| p.storage_backend.as_deref().unwrap_or(id.as_str()) == backend)
            .map(|(id, _)| id.clone())
    }

    /// Sends a request through OAGW; non-2xx upstream responses are errors.
    ///
    /// # Errors
    /// Gateway or upstream failures.
    pub async fn send(
        &self,
        req: http::Request<Body>,
    ) -> Result<http::Response<Body>, ProviderCallError> {
        let ctx = self
            .s2s_ctx()
            .await
            .map_err(|e| ProviderCallError::Provider {
                status: None,
                message: e.to_string(),
                transient: true,
            })?;
        let resp = match self.oagw.proxy_request(ctx, req).await {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(error = %e, "OAGW proxy request failed");
                return Err(map_gateway_error(&e));
            }
        };
        if resp.status().is_success() {
            Ok(resp)
        } else {
            Err(map_upstream_error(resp).await)
        }
    }

    /// Sends a JSON request and parses a JSON response.
    ///
    /// # Errors
    /// Provider failures or invalid JSON.
    pub async fn send_json(
        &self,
        method: http::Method,
        uri: &str,
        body: Option<&serde_json::Value>,
        timeout: Duration,
    ) -> Result<serde_json::Value, ProviderCallError> {
        let mut b = http::Request::builder().method(method).uri(uri);
        let body = match body {
            Some(v) => {
                b = b.header(http::header::CONTENT_TYPE, "application/json");
                Body::from(serde_json::to_vec(v).unwrap_or_default())
            }
            None => Body::Empty,
        };
        let req = b.body(body).map_err(|e| ProviderCallError::Provider {
            status: None,
            message: e.to_string(),
            transient: false,
        })?;
        let resp = tokio::time::timeout(timeout, self.send(req))
            .await
            .map_err(|_| ProviderCallError::Timeout("request timed out".to_owned()))??;
        let bytes = tokio::time::timeout(timeout, resp.into_body().into_bytes())
            .await
            .map_err(|_| ProviderCallError::Timeout("response body timed out".to_owned()))?
            .map_err(|e| ProviderCallError::Provider {
                status: None,
                message: e.to_string(),
                transient: true,
            })?;
        if bytes.is_empty() {
            return Ok(serde_json::Value::Null);
        }
        serde_json::from_slice(&bytes).map_err(|e| ProviderCallError::Provider {
            status: None,
            message: format!("invalid provider response: {e}"),
            transient: false,
        })
    }

    /// Sends a DELETE; 2xx and 404 are success.
    ///
    /// # Errors
    /// Any other status or transport failure.
    pub async fn delete(&self, uri: &str, timeout: Duration) -> Result<(), ProviderCallError> {
        let req = http::Request::builder()
            .method(http::Method::DELETE)
            .uri(uri)
            .body(Body::Empty)
            .map_err(|e| ProviderCallError::Provider {
                status: None,
                message: e.to_string(),
                transient: false,
            })?;
        match tokio::time::timeout(timeout, self.send(req)).await {
            Err(_) => Err(ProviderCallError::Timeout("delete timed out".to_owned())),
            // Already gone (404) counts as deleted.
            Ok(
                Ok(_)
                | Err(ProviderCallError::Provider {
                    status: Some(404), ..
                }),
            ) => Ok(()),
            Ok(Err(e)) => Err(e),
        }
    }
}
