//! The three plugin traits and the request/response contexts they operate on (ADR-0002).

use axum::http::{HeaderMap, Method, StatusCode};
use serde_json::Value;

/// A proxied request as the plugin chain sees it.
#[derive(Debug, Clone)]
pub struct ProxyRequest {
    /// Method of the outbound request.
    pub method: Method,
    /// Path sent upstream (route path plus suffix).
    pub path: String,
    /// Encoded query string sent upstream (already filtered by the allowlist).
    pub query: String,
    /// Headers of the outbound request.
    pub headers: HeaderMap,
    /// Buffered body of the outbound request.
    pub body: bytes::Bytes,
    /// Tenant of the caller.
    pub tenant_id: uuid::Uuid,
    /// Security context of the caller, used when resolving credentials.
    pub security: Option<std::sync::Arc<toolkit_security::SecurityContext>>,
}

impl ProxyRequest {
    /// Value of the first header named `name`, ASCII-lowercased for comparisons.
    #[must_use]
    pub fn header(&self, name: &str) -> Option<String> {
        self.headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string)
    }

    /// Replace or insert a header value.
    pub fn set_header(&mut self, name: &str, value: &str) {
        if let Ok(name) = axum::http::HeaderName::from_bytes(name.as_bytes())
            && let Ok(value) = axum::http::HeaderValue::from_str(value) {
                self.headers.insert(name, value);
            }
    }

    /// Remove a header.
    pub fn remove_header(&mut self, name: &str) {
        if let Ok(name) = axum::http::HeaderName::from_bytes(name.as_bytes()) {
            self.headers.remove(name);
        }
    }

    /// Append a header, keeping any existing values.
    pub fn add_header(&mut self, name: &str, value: &str) {
        if let Ok(name) = axum::http::HeaderName::from_bytes(name.as_bytes())
            && let Ok(value) = axum::http::HeaderValue::from_str(value) {
                self.headers.append(name, value);
            }
    }
}

/// A proxied response as the plugin chain sees it.
#[derive(Debug, Clone)]
pub struct ProxyResponse {
    /// Status returned by the upstream.
    pub status: StatusCode,
    /// Headers of the upstream response.
    pub headers: HeaderMap,
    /// Buffered body, when the response is not streaming.
    pub body: bytes::Bytes,
}

/// Rejection produced by a guard plugin.
#[derive(Debug, Clone)]
pub struct GuardDecision {
    /// HTTP status to answer with.
    pub status: StatusCode,
    /// Machine-readable error code.
    pub code: String,
    /// Human-readable explanation.
    pub detail: String,
}

impl GuardDecision {
    /// A rejection with the given status, code and detail.
    #[must_use]
    pub fn reject(status: StatusCode, code: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            status,
            code: code.into(),
            detail: detail.into(),
        }
    }
}

/// Error raised inside the plugin chain.
#[derive(Debug, thiserror::Error)]
pub enum PluginError {
    /// The credential store could not resolve a reference.
    #[error("referenced secret '{0}' could not be resolved")]
    SecretNotFound(String),
    /// Authentication against the upstream failed.
    #[error("authentication to the upstream failed: {0}")]
    AuthFailed(String),
    /// A guard rejected the request, with its machine-readable error code.
    #[error("{message}")]
    Guard {
        /// Code the guard rejected with, e.g. `REQUIRED_HEADER_MISSING` (ADR-0009).
        code: String,
        /// Human explanation of the rejection.
        message: String,
    },
    /// A guard rejected the response, with its machine-readable error code.
    #[error("{message}")]
    ResponseGuard {
        /// Code the guard rejected with, e.g. `REQUIRED_HEADER_MISSING`.
        code: String,
        /// Human explanation of the rejection.
        message: String,
    },
    /// The plugin configuration is unusable.
    #[error("{0}")]
    Config(String),
    /// Something else went wrong.
    #[error("{0}")]
    Other(String),
}

/// Authentication plugin: injects credentials into the outbound request (ADR-0002).
#[async_trait::async_trait]
pub trait AuthPlugin: Send + Sync {
    /// GTS identifier of the plugin implementation.
    fn id(&self) -> &'static str;
    /// Human-readable plugin type.
    fn plugin_type(&self) -> &'static str;
    /// Inject credentials into `request`.
    async fn authenticate(
        &self,
        request: &mut ProxyRequest,
        config: &Value,
    ) -> Result<(), PluginError>;
}

/// Policy plugin: validates requests and responses, may reject (ADR-0002).
#[async_trait::async_trait]
pub trait GuardPlugin: Send + Sync {
    /// GTS identifier of the plugin.
    fn id(&self) -> &'static str;
    /// Human-readable plugin type.
    fn plugin_type(&self) -> &'static str;
    /// Validate the outbound request.
    async fn guard_request(
        &self,
        request: &ProxyRequest,
        config: &Value,
    ) -> Result<(), PluginError>;
    /// Validate the upstream response.
    async fn guard_response(
        &self,
        response: &ProxyResponse,
        config: &Value,
    ) -> Result<(), PluginError>;
}

/// Transformation plugin: mutates requests, responses and errors (ADR-0002).
#[async_trait::async_trait]
pub trait TransformPlugin: Send + Sync {
    /// GTS identifier of the plugin.
    fn id(&self) -> &'static str;
    /// Human-readable plugin type.
    fn plugin_type(&self) -> &'static str;
    /// Mutate the outbound request.
    async fn transform_request(
        &self,
        request: &mut ProxyRequest,
        config: &Value,
    ) -> Result<(), PluginError>;
    /// Mutate the upstream response.
    async fn transform_response(
        &self,
        response: &mut ProxyResponse,
        config: &Value,
    ) -> Result<(), PluginError>;
    /// Mutate a gateway error before it is rendered.
    async fn transform_error(
        &self,
        detail: &mut String,
        config: &Value,
    ) -> Result<(), PluginError>;
}

/// Convert a [`PluginError`] into the matching [`crate::domain::error::DomainError`].
#[must_use]
pub fn plugin_error(e: PluginError) -> crate::domain::error::DomainError {
    match e {
        PluginError::SecretNotFound(name) => crate::domain::error::DomainError::SecretNotFound(name),
        PluginError::AuthFailed(detail) => crate::domain::error::DomainError::AuthFailed(detail),
        PluginError::Guard { code, message } => crate::domain::error::DomainError::Guard { code, message },
        PluginError::ResponseGuard { code, message } => {
            crate::domain::error::DomainError::ResponseGuard { code, message }
        }
        PluginError::Config(detail) | PluginError::Other(detail) => {
            crate::domain::error::DomainError::Internal(detail)
        }
    }
}
