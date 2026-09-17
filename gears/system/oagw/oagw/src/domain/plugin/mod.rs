//! Plugin contracts of the data plane (ADR-0002).
//!
//! Three traits cover the three plugin purposes — credential injection
//! ([`AuthPlugin`]), policy enforcement ([`GuardPlugin`]) and request/response
//! mutation ([`TransformPlugin`]). Every trait method receives the shared
//! [`RequestContext`]/[`ResponseContext`]/[`ErrorContext`] of the request it is
//! part of; the plugin binding's configuration is carried in
//! [`RequestContext::config`], which the chain executor refreshes before each
//! plugin runs.
//!
//! Plugins are constructed per request from the resolved binding (see
//! [`crate::infra::proxy`] and the built-ins under
//! [`crate::infra::proxy::builtins`]).

use std::collections::BTreeMap;

use async_trait::async_trait;
use axum::http::HeaderMap;
use bytes::Bytes;
use serde_json::Value;
use uuid::Uuid;

use crate::domain::error::ErrorKind;

/// The mutable state of a request as it travels the plugin chain.
#[derive(Debug, Clone)]
pub struct RequestContext {
    /// Tenant of the caller.
    pub tenant_id: Uuid,
    /// Subject id of the caller.
    pub subject_id: Uuid,
    /// HTTP method of the request.
    pub method: String,
    /// Path forwarded upstream (`match.http.path` plus the suffix).
    pub path: String,
    /// Query string as received (without the `?`).
    pub query: String,
    /// Outbound headers, already stripped of routing and hop-by-hop headers.
    pub headers: HeaderMap,
    /// Request body as received.
    pub body: Bytes,
    /// Configuration of the plugin currently executing.
    pub config: Value,
    /// Scratch values shared between the plugins of one request.
    pub attributes: BTreeMap<String, String>,
}

impl RequestContext {
    /// Reads a scratch attribute.
    #[must_use]
    pub fn attribute(&self, key: &str) -> Option<&str> {
        self.attributes.get(key).map(String::as_str)
    }

    /// Writes a scratch attribute, overwriting any previous value.
    pub fn set_attribute(&mut self, key: impl Into<String>, value: impl Into<String>) {
        self.attributes.insert(key.into(), value.into());
    }
}

/// The upstream response as it travels back through the plugin chain.
#[derive(Debug, Clone)]
pub struct ResponseContext {
    /// Status the upstream answered with.
    pub status: u16,
    /// Response headers, with hop-by-hop headers already stripped.
    pub headers: HeaderMap,
    /// Response body as received so far.
    pub body: Bytes,
    /// Configuration of the plugin currently executing.
    pub config: Value,
}

/// A gateway error as it travels back through the plugin chain.
#[derive(Debug, Clone)]
pub struct ErrorContext {
    /// Category of the gateway error.
    pub kind: ErrorKind,
    /// Human readable explanation.
    pub detail: String,
    /// Response headers attached to the error (`Retry-After`, `X-RateLimit-*`).
    pub headers: Vec<(String, String)>,
    /// Configuration of the plugin currently executing.
    pub config: Value,
}

impl ErrorContext {
    /// Builds an error context from a domain error.
    #[must_use]
    pub fn from_error(error: &crate::domain::error::DomainError) -> Self {
        Self {
            kind: error.kind,
            detail: error.detail.clone(),
            headers: error.headers.clone(),
            config: Value::Null,
        }
    }
}

/// Credential material handed to a plugin without ever crossing a log line.
///
/// Wraps [`toolkit_auth::oauth2::SecretString`] so plugins depend on the
/// plugin contract rather than on the auth crate directly.
#[derive(Clone)]
pub struct Secret(String);

impl Secret {
    /// Wraps a plain value.
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// Reads the secret; callers must not log or persist the result.
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("[REDACTED]")
    }
}

/// Resolves `cred://` references to credential material.
///
/// The data plane never sees plaintext secrets in configuration: plugins name
/// them with a `secret_ref` and resolve them here. Values that are not
/// `cred://` references are literals and are returned as-is, which keeps the
/// built-ins usable without a wired credential store.
#[async_trait]
pub trait SecretResolver: Send + Sync {
    /// Resolves a secret reference or literal.
    ///
    /// # Errors
    /// Returns [`PluginError::not_found`] with error code
    /// `SECRET_NOT_FOUND` when a reference cannot be resolved.
    async fn resolve(&self, secret_ref: &str) -> Result<Secret, PluginError>;
}

/// A resolver that only understands literal values.
///
/// Every `cred://` reference fails with `SECRET_NOT_FOUND` (500); the gear
/// ships no credential store integration, so named credentials are reported
/// as missing rather than silently dropped.
#[derive(Debug, Default)]
pub struct LiteralSecretResolver;

#[async_trait]
impl SecretResolver for LiteralSecretResolver {
    async fn resolve(&self, secret_ref: &str) -> Result<Secret, PluginError> {
        if secret_ref.starts_with("cred://") {
            return Err(PluginError::not_found(
                "SECRET_NOT_FOUND",
                format!("credential {secret_ref:?} is not available to this gateway"),
            ));
        }
        Ok(Secret::new(secret_ref))
    }
}

/// Why a plugin failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginError {
    /// HTTP status the caller should see.
    pub status: u16,
    /// Machine readable error code (`REQUIRED_HEADER_MISSING`, ...).
    pub error_code: String,
    /// Human readable explanation.
    pub detail: String,
}

impl PluginError {
    /// Builds a plugin error for a status code.
    #[must_use]
    pub fn new(status: u16, error_code: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            status,
            error_code: error_code.into(),
            detail: detail.into(),
        }
    }

    /// A 400 validation failure.
    #[must_use]
    pub fn invalid(code: impl Into<String>, detail: impl Into<String>) -> Self {
        Self::new(400, code, detail)
    }

    /// A 401 authentication failure.
    #[must_use]
    pub fn unauthorized(detail: impl Into<String>) -> Self {
        Self::new(401, "AUTHENTICATION_FAILED", detail)
    }

    /// A 404/500 lookup failure.
    #[must_use]
    pub fn not_found(code: impl Into<String>, detail: impl Into<String>) -> Self {
        Self::new(500, code, detail)
    }

    /// A 502 upstream failure.
    #[must_use]
    pub fn upstream(code: impl Into<String>, detail: impl Into<String>) -> Self {
        Self::new(502, code, detail)
    }
}

impl std::fmt::Display for PluginError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({}): {}", self.error_code, self.status, self.detail)
    }
}

impl std::error::Error for PluginError {}

/// Maps a plugin failure status to the gateway error kind.
#[must_use]
pub fn error_kind_of(status: u16) -> ErrorKind {
    match status {
        400 => ErrorKind::ValidationError,
        401 => ErrorKind::AuthenticationFailed,
        403 => ErrorKind::CorsOriginNotAllowed,
        404 => ErrorKind::RouteNotFound,
        413 => ErrorKind::PayloadTooLarge,
        429 => ErrorKind::RateLimitExceeded,
        500 => ErrorKind::SecretNotFound,
        502 => ErrorKind::DownstreamError,
        503 => ErrorKind::PluginNotFound,
        504 => ErrorKind::RequestTimeout,
        other if other < 500 => ErrorKind::ValidationError,
        _ => ErrorKind::DownstreamError,
    }
}

/// Verdict of a guard plugin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GuardDecision {
    /// The request/response is accepted.
    Allow,
    /// The request (or response) is rejected.
    Reject {
        /// HTTP status to answer with.
        status: u16,
        /// Machine readable error code.
        error_code: String,
        /// Human readable explanation.
        detail: String,
    },
}

impl GuardDecision {
    /// Whether the decision is [`GuardDecision::Allow`].
    #[must_use]
    pub const fn is_allow(&self) -> bool {
        matches!(self, Self::Allow)
    }
}

/// Credential injection plugin; at most one per upstream (ADR-0002).
#[async_trait]
pub trait AuthPlugin: Send + Sync {
    /// Instance identifier of this plugin.
    fn id(&self) -> &str;

    /// GTS base type segment of the plugin (`cf.core.oagw.auth_plugin.v1`).
    fn plugin_type(&self) -> &str;

    /// Injects credentials into `ctx.headers`.
    ///
    /// # Errors
    /// Returns [`PluginError`] when credentials cannot be resolved or the
    /// request cannot be authenticated.
    async fn authenticate(&self, ctx: &mut RequestContext) -> Result<(), PluginError>;
}

/// Request policy enforcement (ADR-0002).
#[async_trait]
pub trait GuardPlugin: Send + Sync {
    /// Instance identifier of this plugin.
    fn id(&self) -> &str;

    /// GTS base type segment of the plugin.
    fn plugin_type(&self) -> &str;

    /// Inspects the outbound request.
    ///
    /// # Errors
    /// Returns [`PluginError`] when the plugin itself fails.
    async fn guard_request(&self, ctx: &RequestContext) -> Result<GuardDecision, PluginError>;

    /// Inspects the upstream response; defaults to allowing it.
    ///
    /// # Errors
    /// Returns [`PluginError`] when the plugin itself fails.
    async fn guard_response(&self, ctx: &ResponseContext) -> Result<GuardDecision, PluginError> {
        let _ = ctx;
        Ok(GuardDecision::Allow)
    }
}

/// Request/response mutation (ADR-0002).
#[async_trait]
pub trait TransformPlugin: Send + Sync {
    /// Instance identifier of this plugin.
    fn id(&self) -> &str;

    /// GTS base type segment of the plugin.
    fn plugin_type(&self) -> &str;

    /// Mutates the outbound request.
    ///
    /// # Errors
    /// Returns [`PluginError`] when the transformation fails.
    async fn transform_request(&self, ctx: &mut RequestContext) -> Result<(), PluginError>;

    /// Mutates the upstream response; defaults to leaving it untouched.
    ///
    /// # Errors
    /// Returns [`PluginError`] when the transformation fails.
    async fn transform_response(&self, ctx: &mut ResponseContext) -> Result<(), PluginError> {
        let _ = ctx;
        Ok(())
    }

    /// Mutates a gateway error before it is rendered; defaults to leaving it
    /// untouched.
    ///
    /// # Errors
    /// Returns [`PluginError`] when the transformation fails.
    async fn transform_error(&self, ctx: &mut ErrorContext) -> Result<(), PluginError> {
        let _ = ctx;
        Ok(())
    }
}
