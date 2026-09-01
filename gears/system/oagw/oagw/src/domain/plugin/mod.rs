// Created: 2026-08-29 by Constructor Tech
//! Plugin traits and the plugin-chain execution model.
//!
//! Three plugin types with deterministic execution order (ADR-0002):
//! `Auth` → `Guard` → `Transform(on_request)` → upstream →
//! `Transform(on_response/on_error)`.

use async_trait::async_trait;
use axum::http::{HeaderMap, StatusCode, Uri};

use super::error::OagwError;

pub mod catalog;

pub use catalog::plugin_instance_matches;

/// Auth plugin instance part of the always-success no-op.
pub const AUTH_NOOP: &str = "cf.core.oagw.noop.v1";
/// Auth plugin instance part of the `cred_store`-backed API key.
pub const AUTH_APIKEY: &str = "cf.core.oagw.apikey.v1";
/// Auth plugin instance part of the OAuth2 client-credentials (form) plugin.
pub const AUTH_OAUTH2_CC: &str = "cf.core.oagw.oauth2_client_cred.v1";
/// Auth plugin instance part of the OAuth2 client-credentials (basic) plugin.
pub const AUTH_OAUTH2_CC_BASIC: &str = "cf.core.oagw.oauth2_client_cred_basic.v1";

/// Guard plugin instance part of the required-headers guard.
pub const GUARD_REQUIRED_HEADERS: &str = "cf.core.oagw.required_headers.v1";

/// Transform plugin instance part of the request-id propagation plugin.
pub const TRANSFORM_REQUEST_ID: &str = "cf.core.oagw.request_id.v1";

/// Auth plugins that have a backing `AuthPlugin` implementation.
const IMPLEMENTABLE_AUTH: [&str; 4] =
    [AUTH_NOOP, AUTH_APIKEY, AUTH_OAUTH2_CC, AUTH_OAUTH2_CC_BASIC];

/// Reserved guard/transform identifiers that are core data-plane logic, not
/// registry-resolvable plugins (ADR-0002). The PRD marks them "not
/// `plugins`-bindable", but a chain that names one is accepted at
/// configuration time and fails with `503 PluginNotFound` at proxy time.
const CATALOG_ONLY: [&str; 4] = [
    "cf.core.oagw.timeout.v1",
    "cf.core.oagw.cors.v1",
    "cf.core.oagw.logging.v1",
    "cf.core.oagw.metrics.v1",
];

/// Reserved auth identifiers with no backing implementation (PRD). Configuring
/// one on an upstream is refused at configuration time: unlike a chain entry it
/// is the only credential source, so no request could ever succeed.
const CATALOG_ONLY_AUTH: [&str; 2] = ["cf.core.oagw.basic.v1", "cf.core.oagw.bearer.v1"];

/// `true` when `instance` resolves to a built-in auth plugin implementation.
#[must_use]
pub fn is_implementable_auth_plugin(instance: &str) -> bool {
    IMPLEMENTABLE_AUTH.contains(&instance)
}

/// `true` when `instance` is a reserved auth id with no implementation.
#[must_use]
pub fn is_catalog_only_plugin(instance: &str) -> bool {
    CATALOG_ONLY_AUTH.contains(&instance)
}

/// `true` when `instance` is a reserved guard/transform id the registries do
/// not resolve.
#[must_use]
pub fn is_catalog_only_chain_plugin(instance: &str) -> bool {
    CATALOG_ONLY.contains(&instance)
}

/// `true` when `instance` names something the catalog or a registry knows.
///
/// Config-time plugin-chain validation accepts these and rejects everything
/// else; custom plugin instances (bare UUIDs) are checked separately.
#[must_use]
pub fn is_known_plugin(instance: &str) -> bool {
    is_implementable_auth_plugin(instance)
        || is_implementable_guard_plugin(instance)
        || is_implementable_transform_plugin(instance)
        || is_catalog_only_chain_plugin(instance)
}

/// `true` when `instance` is a registered built-in guard plugin.
#[must_use]
pub fn is_implementable_guard_plugin(instance: &str) -> bool {
    instance == GUARD_REQUIRED_HEADERS
}

/// `true` when `instance` is a registered built-in transform plugin.
#[must_use]
pub fn is_implementable_transform_plugin(instance: &str) -> bool {
    instance == TRANSFORM_REQUEST_ID
}

/// Result of a guard plugin evaluation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GuardDecision {
    /// Continue the chain.
    Allow,
    /// Stop the chain with the given wire error.
    Reject {
        /// HTTP status for the rejection.
        status: StatusCode,
        /// Machine readable error code (e.g. `REQUIRED_HEADER_MISSING`).
        error_code: String,
        /// Human readable explanation.
        message: String,
    },
}

/// Failure surfaced by a plugin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PluginError {
    /// Authentication failed → `401 AuthenticationFailed`.
    Authentication(String),
    /// Policy rejection with an explicit wire status.
    Rejected {
        /// HTTP status for the rejection.
        status: StatusCode,
        /// Machine readable error code.
        error_code: String,
        /// Human readable explanation.
        message: String,
    },
    /// Plugin implementation failure → `503 PluginNotFound` /
    /// `500 Internal`.
    Internal(String),
}

impl From<PluginError> for OagwError {
    fn from(err: PluginError) -> Self {
        match err {
            PluginError::Authentication(msg) => Self::AuthenticationFailed(msg),
            PluginError::Rejected {
                status,
                error_code,
                message,
            } => {
                let detail = format!("{error_code}: {message}");
                match status.as_u16() {
                    400 => Self::Validation(detail),
                    401 => Self::AuthenticationFailed(detail),
                    403 => Self::CorsOriginNotAllowed(detail),
                    404 => Self::RouteNotFound(detail),
                    502 => Self::DownstreamError(detail),
                    503 => Self::PluginNotFound(detail),
                    _ => Self::Internal(detail),
                }
            }
            PluginError::Internal(msg) => Self::Internal(msg),
        }
    }
}

/// Request-side plugin context.
#[derive(Debug)]
pub struct RequestContext {
    /// Tenant that owns the resolved upstream.
    pub tenant_id: uuid::Uuid,
    /// Resolved upstream id.
    pub upstream_id: uuid::Uuid,
    /// Upstream alias used for routing.
    pub alias: String,
    /// Request method.
    pub method: String,
    /// Target path (route match path plus suffix).
    pub path: String,
    /// Raw query string (already allowlist filtered).
    pub query: Option<String>,
    /// Mutable request headers.
    pub headers: HeaderMap,
    /// Request body (buffered; the data plane enforces the 100 MiB limit).
    pub body: bytes::Bytes,
    /// Inbound request URI (used for `instance` in problem documents).
    pub uri: Uri,
    /// Auth plugin configuration object.
    pub config: serde_json::Map<String, serde_json::Value>,
    /// Caller security context, used by credential-resolving plugins only.
    pub security: toolkit_security::SecurityContext,
}

/// Response-side plugin context.
#[derive(Debug)]
pub struct ResponseContext {
    /// Request headers echoed for correlation.
    pub request_headers: HeaderMap,
    /// Mutable response headers.
    pub headers: HeaderMap,
    /// Upstream response status.
    pub status: StatusCode,
    /// Response body (buffered).
    pub body: bytes::Bytes,
    /// Plugin configuration object.
    pub config: serde_json::Map<String, serde_json::Value>,
}

/// Error-phase plugin context.
#[derive(Debug)]
pub struct ErrorContext {
    /// Gateway error produced by the pipeline.
    pub error: OagwError,
    /// Mutable response headers for the synthetic response.
    pub headers: HeaderMap,
    /// Plugin configuration object.
    pub config: serde_json::Map<String, serde_json::Value>,
}

/// Credential injection plugin. One per upstream.
#[async_trait]
pub trait AuthPlugin: Send + Sync {
    /// Plugin instance id (the GTS instance part, e.g. `cf.core.oagw.apikey.v1`).
    fn id(&self) -> &str;

    /// Plugin kind, e.g. `auth_plugin`.
    fn plugin_type(&self) -> &str;

    /// Inject outbound credentials into `ctx.headers`.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError::Authentication`] when the request cannot be
    /// authenticated.
    async fn authenticate(&self, ctx: &mut RequestContext) -> Result<(), PluginError>;
}

/// Validation / policy plugin. Multiple per upstream or route.
#[async_trait]
pub trait GuardPlugin: Send + Sync {
    /// Plugin instance id.
    fn id(&self) -> &str;

    /// Plugin kind, e.g. `guard_plugin`.
    fn plugin_type(&self) -> &str;

    /// Validate the outbound request.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError`] when the guard cannot run.
    async fn guard_request(&self, ctx: &RequestContext) -> Result<GuardDecision, PluginError>;

    /// Validate the upstream response.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError`] when the guard cannot evaluate the response.
    async fn guard_response(&self, ctx: &ResponseContext) -> Result<GuardDecision, PluginError>;
}

/// Request / response mutation plugin. Multiple per upstream or route.
#[async_trait]
pub trait TransformPlugin: Send + Sync {
    /// Plugin instance id.
    fn id(&self) -> &str;

    /// Plugin kind, e.g. `transform_plugin`.
    fn plugin_type(&self) -> &str;

    /// Mutate the outbound request.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError`] when the transform fails.
    async fn transform_request(&self, ctx: &mut RequestContext) -> Result<(), PluginError>;

    /// Mutate the response returned to the caller.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError`] when the transform cannot run.
    async fn transform_response(&self, ctx: &mut ResponseContext) -> Result<(), PluginError>;

    /// Mutate the synthetic error response.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError`] when the transform cannot run.
    async fn transform_error(&self, ctx: &mut ErrorContext) -> Result<(), PluginError>;
}

/// A plugin instance resolved from the registry together with its config.
#[derive(Debug, Clone)]
pub struct PluginBinding {
    /// Canonical plugin reference (`gts.…~<instance>` or a UUID).
    pub plugin_ref: String,
    /// Configuration JSON for the plugin.
    pub config: serde_json::Value,
}

/// Ordered plugin chain after hierarchical merge.
#[derive(Debug, Default)]
pub struct PluginChain {
    /// Auth plugin (at most one).
    pub auth: Option<PluginBinding>,
    /// Guard plugins in execution order.
    pub guards: Vec<PluginBinding>,
    /// Transform plugins in execution order.
    pub transforms: Vec<PluginBinding>,
}
