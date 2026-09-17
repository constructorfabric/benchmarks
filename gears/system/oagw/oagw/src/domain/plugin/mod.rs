//! Plugin traits, request/response contexts and plugin errors (ADR-0002).
//!
//! The three trait signatures are verbatim from the ADR; the contexts carry
//! everything a plugin needs (security context, plugin config, wire message)
//! plus an attribute bag for plugins that communicate downstream.

use std::collections::BTreeMap;

use axum::http::HeaderMap;
use bytes::Bytes;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::domain::error::DomainError;

/// Credential-injection plugin, executed once per request before guards.
#[async_trait::async_trait]
pub trait AuthPlugin: Send + Sync {
    /// Registry id of the plugin (`apikey`, `oauth2_client_cred`, …).
    fn id(&self) -> &str;
    /// GTS instance id of the plugin (`cf.core.oagw.apikey.v1`, …).
    fn plugin_type(&self) -> &str;
    /// Inject credentials into the outbound request.
    ///
    /// # Errors
    ///
    /// Returns a [`PluginError`] when credentials cannot be resolved or
    /// injected.
    async fn authenticate(&self, ctx: &mut RequestContext) -> Result<(), PluginError>;
}

/// Policy-enforcement plugin, executed after auth and before transforms.
#[async_trait::async_trait]
pub trait GuardPlugin: Send + Sync {
    /// Registry id of the plugin.
    fn id(&self) -> &str;
    /// GTS instance id of the plugin.
    fn plugin_type(&self) -> &str;
    /// Validate the outbound request.
    ///
    /// # Errors
    ///
    /// Returns a [`PluginError`] when the plugin itself fails.
    async fn guard_request(&self, ctx: &RequestContext) -> Result<GuardDecision, PluginError>;
    /// Validate the upstream response.
    ///
    /// # Errors
    ///
    /// Returns a [`PluginError`] when the plugin itself fails.
    async fn guard_response(&self, ctx: &ResponseContext) -> Result<GuardDecision, PluginError>;
}

/// Request/response/error mutation plugin, executed around the upstream call.
#[async_trait::async_trait]
pub trait TransformPlugin: Send + Sync {
    /// Registry id of the plugin.
    fn id(&self) -> &str;
    /// GTS instance id of the plugin.
    fn plugin_type(&self) -> &str;
    /// Mutate the outbound request.
    ///
    /// # Errors
    ///
    /// Returns a [`PluginError`] when the plugin itself fails.
    async fn transform_request(&self, ctx: &mut RequestContext) -> Result<(), PluginError>;
    /// Mutate the client response.
    ///
    /// # Errors
    ///
    /// Returns a [`PluginError`] when the plugin itself fails.
    async fn transform_response(&self, ctx: &mut ResponseContext) -> Result<(), PluginError>;
    /// Mutate a gateway-generated error response.
    ///
    /// # Errors
    ///
    /// Returns a [`PluginError`] when the plugin itself fails.
    async fn transform_error(&self, ctx: &mut ErrorContext) -> Result<(), PluginError>;
}

/// Identity of the caller, as presented to plugins.
#[derive(Clone)]
pub struct Caller {
    /// Tenant of the authenticated subject.
    pub tenant_id: Uuid,
    /// Identifier of the authenticated subject.
    pub subject_id: Uuid,
    /// Platform security context handed to credstore and authz calls.
    pub security_context: std::sync::Arc<SecurityContext>,
}

impl Caller {
    /// Build a caller from a platform security context.
    #[must_use]
    pub fn from_context(context: &SecurityContext) -> Self {
        Self {
            tenant_id: context.subject_tenant_id(),
            subject_id: context.subject_id(),
            security_context: std::sync::Arc::new(context.clone()),
        }
    }
}

/// Per-request state handed to auth, guard and transform plugins.
#[derive(Clone)]
pub struct RequestContext {
    /// Caller identity.
    pub caller: Caller,
    /// Configuration of the plugin being executed.
    pub config: serde_json::Value,
    /// Inbound request method.
    pub method: String,
    /// Outbound request path (already rewritten for the route).
    pub path: String,
    /// Query parameters allowed through by the route match.
    pub query: Vec<(String, String)>,
    /// Outbound request headers (mutable for auth/transform plugins).
    pub headers: HeaderMap,
    /// Request body.
    pub body: Bytes,
    /// Cross-plugin communication bag (`request_id`, rate-limit headers, …).
    pub attributes: BTreeMap<String, String>,
}

impl RequestContext {
    /// First value of a case-insensitive header, if present.
    #[must_use]
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|value| value.to_str().ok())
    }

    /// Read a string config key, falling back to the attribute bag.
    #[must_use]
    pub fn config_str(&self, key: &str) -> Option<&str> {
        self.config.get(key).and_then(serde_json::Value::as_str)
    }
}

/// Upstream response handed to guard and transform plugins.
#[derive(Clone)]
pub struct ResponseContext {
    /// Caller identity.
    pub caller: Caller,
    /// Configuration of the plugin being executed.
    pub config: serde_json::Value,
    /// Upstream response status.
    pub status: u16,
    /// Upstream response headers (mutable for transform plugins).
    pub headers: HeaderMap,
    /// Upstream response body.
    pub body: Bytes,
    /// Cross-plugin communication bag.
    pub attributes: BTreeMap<String, String>,
}

/// Gateway error handed to `transform_error` plugins.
#[derive(Clone)]
pub struct ErrorContext {
    /// Caller identity.
    pub caller: Caller,
    /// Configuration of the plugin being executed.
    pub config: serde_json::Value,
    /// Status the gateway will return.
    pub status: u16,
    /// Error body rendered as an RFC 9457 problem document.
    pub body: Bytes,
    /// Cross-plugin communication bag.
    pub attributes: BTreeMap<String, String>,
}

/// A guard plugin's verdict.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuardDecision {
    /// Whether the request or response may proceed.
    pub allowed: bool,
    /// Status to return when rejected.
    pub status: u16,
    /// Machine-readable rejection code.
    pub error_code: String,
    /// Human-readable explanation.
    pub detail: String,
}

impl GuardDecision {
    /// Verdict that lets the request proceed.
    #[must_use]
    pub const fn allow() -> Self {
        Self {
            allowed: true,
            status: 200,
            error_code: String::new(),
            detail: String::new(),
        }
    }

    /// Verdict that stops the request with `status`.
    #[must_use]
    pub fn reject(status: u16, error_code: &str, detail: impl Into<String>) -> Self {
        Self {
            allowed: false,
            status,
            error_code: error_code.to_owned(),
            detail: detail.into(),
        }
    }
}

/// Failure raised by a plugin implementation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PluginError {
    /// Malformed or missing plugin configuration.
    Config(String),
    /// A referenced secret could not be resolved.
    Secret(String),
    /// Credential injection failed.
    Auth(String),
    /// Unspecified plugin-internal failure.
    Internal(String),
}

impl PluginError {
    /// Map this error onto the contract error catalog.
    #[must_use]
    pub fn to_domain_error(&self) -> DomainError {
        match self {
            Self::Config(detail) => DomainError::validation(detail.clone()),
            Self::Secret(detail) => DomainError::secret_not_found(detail.clone()),
            Self::Auth(detail) => DomainError::auth_failed(detail.clone()),
            Self::Internal(detail) => DomainError::new(
                crate::domain::error::ErrorKind::ProtocolError,
                detail.clone(),
            ),
        }
    }
}

impl std::fmt::Display for PluginError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Config(detail)
            | Self::Secret(detail)
            | Self::Auth(detail)
            | Self::Internal(detail) => write!(f, "{detail}"),
        }
    }
}

impl std::error::Error for PluginError {}

/// Map a guard rejection onto the contract error catalog.
///
/// A `required_headers` rejection reports `400`/`502` with the plugin's own
/// error code in `invalid_value`; CORS rejections keep their dedicated ids.
#[must_use]
pub fn guard_rejection(decision: &GuardDecision) -> DomainError {
    let kind = match decision.status {
        502 => crate::domain::error::ErrorKind::ProtocolError,
        _ => crate::domain::error::ErrorKind::Validation,
    };
    DomainError::new(kind, decision.detail.clone()).with_extensions(
        crate::domain::error::ErrorExtensions {
            invalid_value: Some(decision.error_code.clone()),
            ..crate::domain::error::ErrorExtensions::default()
        },
    )
}

#[cfg(test)]
#[path = "plugin_tests.rs"]
mod plugin_tests;
