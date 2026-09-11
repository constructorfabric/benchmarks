//! Plugin traits and the contexts handed to them (ADR 0002).
//!
//! Execution order is fixed: Auth → Guards → Transform(on_request) → upstream
//! call → Transform(on_response / on_error). Upstream-bound plugins run before
//! route-bound ones.

use async_trait::async_trait;
use bytes::Bytes;
use http::{HeaderMap, Method, StatusCode};
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::domain::error::{ErrorKind, OagwError};
use crate::domain::model::ConfigMap;

/// The outbound request as it is being assembled.
#[derive(Debug, Clone)]
pub struct ProxyRequest {
    pub method: Method,
    /// Absolute path on the upstream, always starting with `/`.
    pub path: String,
    /// Query parameters in order, already filtered by the route allowlist.
    pub query: Vec<(String, String)>,
    pub headers: HeaderMap,
    pub body: Bytes,
}

impl ProxyRequest {
    /// `path` plus the encoded query string, ready for a request line.
    #[must_use]
    pub fn path_and_query(&self) -> String {
        if self.query.is_empty() {
            return self.path.clone();
        }
        let encoded = form_urlencoded::Serializer::new(String::new())
            .extend_pairs(self.query.iter().map(|(k, v)| (k.as_str(), v.as_str())))
            .finish();
        format!("{}?{}", self.path, encoded)
    }
}

/// The upstream response head, before it is written back to the client.
#[derive(Debug, Clone)]
pub struct ProxyResponseHead {
    pub status: StatusCode,
    pub headers: HeaderMap,
}

/// Ambient identity of the request every plugin phase shares.
#[derive(Debug, Clone, Copy)]
pub struct PluginScope<'a> {
    pub security_context: &'a SecurityContext,
    pub alias: &'a str,
    pub upstream_id: Uuid,
    pub route_id: Option<Uuid>,
}

/// Context for [`AuthPlugin::authenticate`].
pub struct AuthContext<'a> {
    pub scope: PluginScope<'a>,
    pub config: &'a ConfigMap,
    pub headers: &'a mut HeaderMap,
    pub query: &'a mut Vec<(String, String)>,
}

impl AuthContext<'_> {
    #[must_use]
    pub fn security_context(&self) -> &SecurityContext {
        self.scope.security_context
    }
}

/// Context for the request phase of guards and transforms.
pub struct RequestContext<'a> {
    pub scope: PluginScope<'a>,
    pub config: &'a ConfigMap,
    pub request: &'a mut ProxyRequest,
}

/// Context for the response phase of guards and transforms.
pub struct ResponseContext<'a> {
    pub scope: PluginScope<'a>,
    pub config: &'a ConfigMap,
    pub response: &'a mut ProxyResponseHead,
}

/// Context for [`TransformPlugin::transform_error`].
pub struct ErrorContext<'a> {
    pub scope: PluginScope<'a>,
    pub config: &'a ConfigMap,
    pub error: &'a mut OagwError,
}

/// Outcome of a guard phase.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GuardDecision {
    /// Continue the chain.
    Allow,
    /// Stop and answer the client with this status.
    Reject {
        status: StatusCode,
        error_code: String,
        message: String,
    },
}

impl GuardDecision {
    #[must_use]
    pub fn reject(status: StatusCode, error_code: &str, message: impl Into<String>) -> Self {
        Self::Reject {
            status,
            error_code: error_code.to_owned(),
            message: message.into(),
        }
    }
}

/// Failure modes a plugin can report.
#[derive(Debug, Clone, thiserror::Error)]
pub enum PluginError {
    /// Credential preparation failed — the caller is not authenticated to the
    /// upstream.
    #[error("authentication failed: {0}")]
    Unauthenticated(String),
    /// A `cred://` reference did not resolve.
    #[error("secret not found: {0}")]
    SecretNotFound(String),
    /// The plugin's own configuration is unusable.
    #[error("invalid plugin configuration: {0}")]
    InvalidConfig(String),
    /// No implementation is registered for the requested identifier.
    #[error("plugin not found: {0}")]
    NotFound(String),
    /// Anything else — never carries secret material.
    #[error("plugin failure: {0}")]
    Internal(String),
}

impl From<PluginError> for OagwError {
    fn from(err: PluginError) -> Self {
        let detail = err.to_string();
        match err {
            PluginError::Unauthenticated(_) => {
                Self::new(ErrorKind::AuthenticationFailed, detail)
            }
            PluginError::SecretNotFound(_) => Self::new(ErrorKind::SecretNotFound, detail),
            PluginError::InvalidConfig(_) => Self::new(ErrorKind::ValidationError, detail),
            PluginError::NotFound(_) => Self::new(ErrorKind::PluginNotFound, detail),
            PluginError::Internal(_) => Self::new(ErrorKind::Internal, detail),
        }
    }
}

/// Credential injection. At most one per upstream.
#[async_trait]
pub trait AuthPlugin: Send + Sync {
    /// Short symbolic name, e.g. `apikey`.
    fn id(&self) -> &str;
    /// Full GTS identifier this plugin is registered under.
    fn plugin_type(&self) -> &str;
    /// Inject credentials into the outbound request.
    ///
    /// # Errors
    ///
    /// Returns a [`PluginError`] when credentials cannot be prepared.
    async fn authenticate(&self, ctx: &mut AuthContext<'_>) -> Result<(), PluginError>;
}

/// Validation and policy enforcement. May reject a request.
#[async_trait]
pub trait GuardPlugin: Send + Sync {
    fn id(&self) -> &str;
    fn plugin_type(&self) -> &str;

    /// Inspect the outbound request.
    ///
    /// # Errors
    ///
    /// Returns a [`PluginError`] when the check itself could not run.
    async fn guard_request(&self, ctx: &RequestContext<'_>)
    -> Result<GuardDecision, PluginError>;

    /// Inspect the upstream response.
    ///
    /// # Errors
    ///
    /// Returns a [`PluginError`] when the check itself could not run.
    async fn guard_response(
        &self,
        ctx: &ResponseContext<'_>,
    ) -> Result<GuardDecision, PluginError>;
}

/// Request/response/error mutation.
#[async_trait]
pub trait TransformPlugin: Send + Sync {
    fn id(&self) -> &str;
    fn plugin_type(&self) -> &str;

    /// Mutate the outbound request.
    ///
    /// # Errors
    ///
    /// Returns a [`PluginError`] on failure.
    async fn transform_request(&self, ctx: &mut RequestContext<'_>) -> Result<(), PluginError>;

    /// Mutate the upstream response.
    ///
    /// # Errors
    ///
    /// Returns a [`PluginError`] on failure.
    async fn transform_response(&self, ctx: &mut ResponseContext<'_>) -> Result<(), PluginError>;

    /// Mutate a gateway error before it is rendered.
    ///
    /// # Errors
    ///
    /// Returns a [`PluginError`] on failure.
    async fn transform_error(&self, ctx: &mut ErrorContext<'_>) -> Result<(), PluginError> {
        let _ = ctx;
        Ok(())
    }
}

/// Read a plugin config value as a string, accepting JSON scalars.
#[must_use]
pub fn config_str(config: &ConfigMap, key: &str) -> Option<String> {
    match config.get(key)? {
        serde_json::Value::String(s) => Some(s.clone()),
        serde_json::Value::Null => None,
        other => Some(other.to_string()),
    }
}

/// Read a plugin config value as a non-blank string.
#[must_use]
pub fn config_nonblank(config: &ConfigMap, key: &str) -> Option<String> {
    config_str(config, key).filter(|s| !s.trim().is_empty())
}

#[cfg(test)]
#[path = "plugin_tests.rs"]
mod tests;
