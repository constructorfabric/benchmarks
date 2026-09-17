//! OAGW plugin contracts (ADR-0002).
//!
//! Three plugin kinds run in a fixed order around the upstream call:
//! `AuthPlugin` (credential injection) → `GuardPlugin` (validation, may
//! reject) → `TransformPlugin` (request/response/error mutation).

use async_trait::async_trait;
use bytes::Bytes;
use http::HeaderMap;
use toolkit_security::SecurityContext;

use crate::domain::error::{OagwError, OagwResult};

/// Execution phase in which a plugin runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PluginPhase {
    /// Before the upstream call.
    Request,
    /// After the upstream call.
    Response,
    /// Instead of a response (error path).
    Error,
}

/// Mutable request state handed to auth and transform plugins.
#[derive(Debug)]
pub struct RequestContext {
    /// Authenticated caller.
    pub security_context: SecurityContext,
    /// Tenant the request resolved against (may be an ancestor).
    pub tenant_id: uuid::Uuid,
    /// Resolved upstream id.
    pub upstream_id: uuid::Uuid,
    /// Matched route id, when a route matched.
    pub route_id: Option<uuid::Uuid>,
    /// Request method.
    pub method: http::Method,
    /// Upstream-bound path.
    pub path: String,
    /// Query parameters as `(name, value)` pairs.
    pub query: Vec<(String, String)>,
    /// Headers as sent upstream.
    pub headers: HeaderMap,
    /// Request body (already size-checked).
    pub body: Bytes,
    /// Plugin configuration for this invocation.
    pub config: serde_json::Value,
}

impl RequestContext {
    /// Returns the first header value for `name`, case-insensitively.
    #[must_use]
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|value| value.to_str().ok())
    }
}

/// Response state handed to guard and transform plugins.
#[derive(Debug)]
pub struct ResponseContext {
    /// Status returned by the upstream.
    pub status: http::StatusCode,
    /// Response headers as returned to the caller.
    pub headers: HeaderMap,
    /// Caller-visible response body (buffered for plugin inspection).
    pub body: Bytes,
    /// `true` when the upstream returned a non-2xx status.
    pub is_error: bool,
    /// Plugin configuration for this invocation (same value the request phase
    /// saw).
    pub config: serde_json::Value,
}

/// Error state handed to transform plugins on the error path.
#[derive(Debug)]
pub struct ErrorContext {
    /// The error OAGW is about to report.
    pub error: OagwError,
    /// Headers accumulated so far for the error response.
    pub headers: HeaderMap,
}

/// Guard verdict.
#[derive(Debug, Clone, PartialEq)]
pub enum GuardDecision {
    /// Continue processing.
    Allow,
    /// Reject with a gateway error.
    Reject(OagwError),
}

impl GuardDecision {
    /// `Allow` unless this is a rejection.
    #[must_use]
    pub fn is_allowed(&self) -> bool {
        matches!(self, Self::Allow)
    }
}

/// Injects authentication credentials into the upstream request (ADR-0002).
///
/// Executed once per request, before guards.
#[async_trait]
pub trait AuthPlugin: Send + Sync {
    /// Plugin identifier (GTS id for built-ins).
    fn id(&self) -> &str;
    /// Plugin kind namespace.
    fn plugin_type(&self) -> &'static str {
        "auth"
    }
    /// Inject credentials into `ctx.headers`.
    ///
    /// # Errors
    ///
    /// Returns [`OagwError::AuthenticationFailed`] when the credential flow
    /// cannot be completed, or [`OagwError::SecretNotFound`] when a referenced
    /// credential is unavailable.
    async fn authenticate(&self, ctx: &mut RequestContext) -> OagwResult<()>;
}

/// Validates requests/responses and may reject them (ADR-0002).
///
/// Executed after auth, before transform.
#[async_trait]
pub trait GuardPlugin: Send + Sync {
    /// Plugin identifier (GTS id for built-ins).
    fn id(&self) -> &str;
    /// Plugin kind.
    fn plugin_type(&self) -> &'static str {
        "guard"
    }
    /// Validate the request before proxying.
    ///
    /// # Errors
    ///
    /// A rejection is returned as [`GuardDecision::Reject`].
    async fn guard_request(&self, ctx: &RequestContext) -> OagwResult<GuardDecision>;
    /// Validate the upstream response before returning it to the caller.
    ///
    /// # Errors
    ///
    /// A rejection is returned as [`GuardDecision::Reject`].
    async fn guard_response(&self, ctx: &ResponseContext) -> OagwResult<GuardDecision>;
}

/// Modifies request/response/error data (ADR-0002).
///
/// Executed before and after the proxy call.
#[async_trait]
pub trait TransformPlugin: Send + Sync {
    /// Plugin identifier (GTS id for built-ins).
    fn id(&self) -> &str;
    /// Plugin kind.
    fn plugin_type(&self) -> &'static str {
        "transform"
    }
    /// Mutate the request before it is sent upstream.
    ///
    /// # Errors
    ///
    /// Returns an [`OagwError`] when the transformation cannot be applied.
    async fn transform_request(&self, ctx: &mut RequestContext) -> OagwResult<()>;
    /// Mutate the response before it is returned to the caller.
    ///
    /// # Errors
    ///
    /// Returns an [`OagwError`] when the transformation cannot be applied.
    async fn transform_response(&self, ctx: &mut ResponseContext) -> OagwResult<()>;
    /// Mutate the error before it is reported.
    ///
    /// # Errors
    ///
    /// Returns an [`OagwError`] when the transformation cannot be applied.
    async fn transform_error(&self, ctx: &mut ErrorContext) -> OagwResult<()>;
}
