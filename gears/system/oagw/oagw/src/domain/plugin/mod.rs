//! Plugin abstraction for the OAGW data plane (ADR 0002).
//!
//! Three plugin categories exist, executed in a fixed order:
//!
//! 1. **Auth** — authenticates with the upstream and prepares credentials.
//! 2. **Guard** — validates the request (`guard_request`) and/or the upstream
//!    response (`guard_response`).
//! 3. **Transform** — mutates the outbound request (`transform_request`),
//!    the upstream response (`transform_response`) and, failing that, the
//!    error response (`transform_error`).
//!
//! Upstream-bound plugins run before route-bound plugins.

use async_trait::async_trait;
use serde_json::Value;
use uuid::Uuid;

/// Failure reason returned by an auth plugin.
#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    /// Credentials were rejected / could not be produced. Maps to
    /// `401 auth.failed.v1`.
    #[error("upstream authentication failed: {0}")]
    Rejected(String),
    /// A referenced credential could not be resolved. Maps to
    /// `500 secret.not_found.v1`.
    #[error("secret not found: {0}")]
    SecretNotFound(String),
    /// Transient backend failure talking to the IdP.
    #[error("auth backend failure: {0}")]
    Backend(String),
}

/// Failure reason returned by a guard plugin.
#[derive(Debug, thiserror::Error)]
pub enum GuardError {
    /// Request phase failure. Maps to `400 validation.error.v1` with the
    /// given `error_code` in `context.error_code`.
    #[error("{detail}")]
    Request { error_code: String, detail: String },
    /// Response phase failure. Maps to `502` with the given `error_code`.
    #[error("{detail}")]
    Response { error_code: String, detail: String },
}

/// Failure reason returned by a transform plugin.
#[derive(Debug, thiserror::Error)]
pub enum TransformError {
    #[error("{0}")]
    Failed(String),
}

/// Shared request context handed to every data-plane plugin.
pub struct RequestContext<'a> {
    /// Inbound (client) request headers, read-only.
    pub headers: &'a http::HeaderMap,
    /// Outbound headers being accumulated for the upstream request.
    pub outbound: http::HeaderMap,
    /// Instance-level config for this plugin binding.
    pub config: &'a Value,
    /// HTTP method of the proxied request.
    pub method: http::Method,
    /// Effective request path forwarded to the upstream.
    pub path: String,
    /// Security context of the calling subject (secret resolution).
    pub security: &'a toolkit_security::SecurityContext,
    /// Owning tenant of the resolved upstream.
    pub tenant_id: Uuid,
}

impl RequestContext<'_> {
    /// Read an inbound header value.
    #[must_use]
    pub fn inbound_header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|v| v.to_str().ok())
    }

    /// Set an outbound header.
    pub fn set_outbound_header(&mut self, name: &str, value: impl Into<String>) {
        if let (Ok(name), Ok(value)) = (
            http::HeaderName::from_bytes(name.as_bytes()),
            http::HeaderValue::from_str(&value.into()),
        ) {
            self.outbound.insert(name, value);
        }
    }

    /// Append an outbound header (allowing duplicates).
    pub fn add_outbound_header(&mut self, name: &str, value: impl Into<String>) {
        if let (Ok(name), Ok(value)) = (
            http::HeaderName::from_bytes(name.as_bytes()),
            http::HeaderValue::from_str(&value.into()),
        ) {
            self.outbound.append(name, value);
        }
    }
}

/// Response context handed to guard/transform response-phase plugins.
pub struct ResponseContext<'a> {
    /// Headers of the upstream response (mutable).
    pub headers: &'a mut http::HeaderMap,
    /// Status of the upstream response.
    pub status: http::StatusCode,
    /// Instance-level config for this plugin binding.
    pub config: &'a Value,
}

impl ResponseContext<'_> {
    /// Set a response header.
    pub fn set_header(&mut self, name: &str, value: impl Into<String>) {
        if let (Ok(name), Ok(value)) = (
            http::HeaderName::from_bytes(name.as_bytes()),
            http::HeaderValue::from_str(&value.into()),
        ) {
            self.headers.insert(name, value);
        }
    }
}

/// Error context handed to transform-error plugins.
pub struct ErrorContext<'a> {
    /// Status of the error response.
    pub status: http::StatusCode,
    /// GTS error type of the problem response.
    pub error_type: String,
    /// Detail text of the problem response.
    pub detail: String,
    /// Instance-level config for this plugin binding.
    pub config: &'a Value,
}

/// Auth plugin — authenticates with the upstream and prepares credentials.
#[async_trait]
pub trait AuthPlugin: Send + Sync {
    /// Stable built-in identifier (matches `gts::AUTH_*` for builtins).
    fn id(&self) -> &'static str;

    /// The `auth_plugin` GTS type this plugin backs.
    fn plugin_type(&self) -> &'static str;

    /// Perform authentication. On success, implementations set the prepared
    /// credential headers on `ctx.outbound` (e.g. `Authorization`).
    async fn authenticate(&self, ctx: &mut RequestContext<'_>) -> Result<(), AuthError>;
}

/// Guard plugin — validates requests and/or upstream responses.
#[async_trait]
pub trait GuardPlugin: Send + Sync {
    /// Stable built-in identifier.
    fn id(&self) -> &'static str;

    /// The `guard_plugin` GTS type this plugin backs.
    fn plugin_type(&self) -> &'static str;

    /// Validate the proxied request. Returning `Err` short-circuits the
    /// pipeline with a `400` problem response.
    async fn guard_request(&self, ctx: &RequestContext<'_>) -> Result<(), GuardError>;

    /// Validate the upstream response. Returning `Err` replaces the response
    /// with a `502` problem response.
    async fn guard_response(&self, ctx: &mut ResponseContext<'_>) -> Result<(), GuardError>;
}

/// Transform plugin — mutates requests, responses, and errors.
#[async_trait]
pub trait TransformPlugin: Send + Sync {
    /// Stable built-in identifier.
    fn id(&self) -> &'static str;

    /// The `transform_plugin` GTS type this plugin backs.
    fn plugin_type(&self) -> &'static str;

    /// Mutate the outbound request (runs before the upstream round-trip).
    async fn transform_request(&self, ctx: &mut RequestContext<'_>) -> Result<(), TransformError>;

    /// Mutate the upstream response (runs before it is sent downstream).
    async fn transform_response(&self, ctx: &mut ResponseContext<'_>) -> Result<(), TransformError>;

    /// Mutate error responses produced by the gateway.
    async fn transform_error(&self, ctx: &mut ErrorContext<'_>) -> Result<(), TransformError>;
}

/// A fully-instantiated plugin with its instance-level config resolved.
pub enum BoundPlugin {
    Auth(Box<dyn AuthPlugin>, Value),
    Guard(Box<dyn GuardPlugin>, Value),
    Transform(Box<dyn TransformPlugin>, Value),
}

impl BoundPlugin {
    #[must_use]
    pub fn config(&self) -> &Value {
        match self {
            Self::Auth(_, c) | Self::Guard(_, c) | Self::Transform(_, c) => c,
        }
    }
}

/// Ordered plugin chain (all bound instances for one request).
#[derive(Default)]
pub struct PluginChain {
    pub auth: Vec<Box<dyn AuthPlugin>>,
    pub guards: Vec<(Box<dyn GuardPlugin>, Value)>,
    pub transforms: Vec<(Box<dyn TransformPlugin>, Value)>,
}

/// A plugin binding site reference: which plugin, with which instance config,
/// bound where.
#[derive(Debug, Clone)]
pub struct PluginBindingRef {
    /// GTS identifier (builtin) or plugin UUID (custom).
    pub plugin_ref: String,
    /// Instance-level config.
    pub config: Value,
    /// Tenant owning the binding (custom-plugin and secret resolution).
    pub tenant_id: Uuid,
}

/// How the data plane plans a plugin pipeline for one request: the resolved
/// plugin-heavy configuration surface.
#[derive(Default)]
pub struct PipelinePlan {
    /// Upstream auth plugin + config (a resolved `upstream.auth`).
    pub auth: Option<(Box<dyn AuthPlugin>, Value)>,
    /// Upstream guards first, then route guards.
    pub guards: Vec<(Box<dyn GuardPlugin>, Value, bool /*from_route*/)>,
    /// Upstream transforms then route transforms. The vec is ordered
    /// upstream-before-route.
    pub transforms: Vec<(Box<dyn TransformPlugin>, Value, bool /*from_route*/)>,
}

/// The plugin registry resolves plugin references (builtin GTS ids and
/// custom UUIDs) into live plugin instances.
#[async_trait::async_trait]
pub trait PluginRegistry: Send + Sync {
    /// Resolve a binding reference into a bound plugin, or `Err` with a
    /// human-readable reason when the plugin is unknown or its category
    /// cannot back the requested phase.
    async fn resolve(&self, binding: &PluginBindingRef) -> Result<BoundPlugin, String>;
}
