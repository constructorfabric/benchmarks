//! Plugin trait definitions (DESIGN §3.2 "Plugin System").
//!
//! Three plugin families with deterministic execution order:
//! Auth → Guards → Transform(request) → upstream call → Transform(response/error).
//! Upstream plugins execute before route plugins.
//!
//! Contexts are plain structs with public fields so plugin authors (including
//! Starlark-backed plugins in the future) need no other imports.

use async_trait::async_trait;
use http::StatusCode;

use toolkit_security::SecurityContext;

/// Error returned by a plugin during request processing.
#[derive(Debug, Clone)]
pub enum PluginError {
    /// The plugin rejects the request; `status` is the HTTP status and
    /// `message` the human-readable reason (mapped to a gateway error).
    Rejected {
        status: StatusCode,
        message: String,
        /// GTS error type override when the plugin rejects with a specific
        /// OAGW error type; `None` maps to `AuthenticationFailed` for 401,
        /// `Validation` for 400 and `Internal` otherwise.
        gts_type: Option<String>,
    },
    /// Plugin configuration is invalid.
    Config { message: String },
    /// Internal plugin failure (never leaks secrets).
    Internal { message: String },
}

impl PluginError {
    /// A rejection that renders as a 401 AuthenticationFailed.
    #[must_use]
    pub fn auth(message: impl Into<String>) -> Self {
        Self::Rejected {
            status: StatusCode::UNAUTHORIZED,
            message: message.into(),
            gts_type: None,
        }
    }
}

/// Request-scoped context shared by Auth, Guard and Transform(request)
/// plugins. Headers and query parameters are exposed as ordered
/// name/value lists so the proxy can rearrange them before forwarding.
pub struct RequestContext<'a> {
    /// Security context of the inbound caller.
    pub security_context: &'a SecurityContext,
    /// Plugin-specific configuration from the upstream/route binding.
    pub config: &'a serde_json::Value,
    /// Inbound HTTP method.
    pub method: &'a http::Method,
    /// Outbound path (matched route path + appended suffix; plugin mutable).
    pub path: String,
    /// Outbound query parameters (plugin mutable).
    pub query: Vec<(String, String)>,
    /// Outbound headers (plugin mutable).
    pub headers: Vec<(String, String)>,
    /// Outbound body (plugin mutable).
    pub body: Option<bytes::Bytes>,
    /// Upstream alias (for logging/metrics).
    pub alias: &'a str,
}

/// Response-scoped context for Transform(response) and guard-response plugins.
pub struct ResponseContext<'a> {
    /// Plugin-specific configuration.
    pub config: &'a serde_json::Value,
    /// Upstream response status.
    pub status: StatusCode,
    /// Response headers (plugin mutable).
    pub headers: Vec<(String, String)>,
    /// Response body (plugin mutable).
    pub body: Option<bytes::Bytes>,
}

/// Error-scoped context for Transform(on_error) plugins.
pub struct ErrorContext<'a> {
    /// Plugin-specific configuration.
    pub config: &'a serde_json::Value,
    /// Error title (never contains secrets).
    pub error: &'a str,
    /// Response headers (plugin mutable).
    pub headers: Vec<(String, String)>,
}

/// Auth plugin: injects credentials into the outbound request.
///
/// One per upstream. `id()` returns the GTS identifier the registry resolves.
#[async_trait]
pub trait AuthPlugin: Send + Sync {
    /// GTS identifier for this plugin.
    fn id(&self) -> &'static str;

    /// Authenticate the outbound request (inject `Authorization` header,
    /// query parameters, etc.). Returning `Err` aborts the proxy request.
    async fn authenticate(&self, ctx: &mut RequestContext<'_>) -> Result<(), PluginError>;
}

/// Guard plugin: validates / policy-enforces; can reject before forwarding.
#[async_trait]
pub trait GuardPlugin: Send + Sync {
    /// GTS identifier for this plugin.
    fn id(&self) -> &'static str;

    /// Validate the outbound request before the upstream call.
    async fn guard_request(&self, ctx: &mut RequestContext<'_>) -> Result<(), PluginError>;

    /// Validate the upstream response before returning it to the client.
    async fn guard_response(&self, _ctx: &mut ResponseContext<'_>) -> Result<(), PluginError> {
        Ok(())
    }
}

/// Transform plugin: mutates request/response/error.
#[async_trait]
pub trait TransformPlugin: Send + Sync {
    /// GTS identifier for this plugin.
    fn id(&self) -> &'static str;

    /// Mutate the outbound request before the upstream call.
    async fn on_request(&self, ctx: &mut RequestContext<'_>) -> Result<(), PluginError>;

    /// Mutate the upstream response before returning it.
    async fn on_response(&self, _ctx: &mut ResponseContext<'_>) -> Result<(), PluginError> {
        Ok(())
    }

    /// Mutate an error response before returning it.
    async fn on_error(&self, _ctx: &mut ErrorContext<'_>) -> Result<(), PluginError> {
        Ok(())
    }
}

/// Boxed plugin instances usable from the registries.
pub type DynAuthPlugin = Arc<dyn AuthPlugin>;
pub type DynGuardPlugin = Arc<dyn GuardPlugin>;
pub type DynTransformPlugin = Arc<dyn TransformPlugin>;

use std::sync::Arc;
