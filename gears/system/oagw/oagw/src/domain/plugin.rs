//! Plugin contracts: the three plugin traits and their context types.
//!
//! See `docs/ADR/0002-plugin-system.md`.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use http::{HeaderMap, StatusCode};
use toolkit_security::SecurityContext;

use crate::domain::error::DomainError;

/// Shared recording sink used to observe plugin execution order.
pub type Trace = Arc<Mutex<Vec<String>>>;

/// Execution phase of a plugin hook.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PluginPhase {
    /// Before the upstream call.
    Request,
    /// After a successful upstream response.
    Response,
    /// After a failed upstream call.
    Error,
}

/// Why a plugin failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PluginErrorKind {
    /// Credential injection or resolution failed (401).
    Authentication,
    /// The plugin rejected the request (400).
    BadRequest,
    /// The plugin rejected the upstream response (502).
    Upstream,
    /// The plugin identifier is unknown to OAGW (503).
    Unresolvable,
}

/// Failure raised by a plugin hook.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{detail}")]
pub struct PluginError {
    /// Why the plugin failed.
    pub kind: PluginErrorKind,
    /// Optional machine-readable code (e.g. `REQUIRED_HEADER_MISSING`).
    pub code: Option<String>,
    /// Human-readable explanation.
    pub detail: String,
}

impl PluginError {
    /// Build a plugin failure.
    #[must_use]
    pub fn new(kind: PluginErrorKind, detail: impl Into<String>) -> Self {
        Self {
            kind,
            code: None,
            detail: detail.into(),
        }
    }

    /// Attach a machine-readable code.
    #[must_use]
    pub fn with_code(mut self, code: impl Into<String>) -> Self {
        self.code = Some(code.into());
        self
    }
}

impl From<&PluginError> for DomainError {
    fn from(err: &PluginError) -> Self {
        match err.kind {
            PluginErrorKind::Authentication => {
                DomainError::AuthenticationFailed(err.detail.clone())
            }
            PluginErrorKind::BadRequest => match &err.code {
                Some(code) => DomainError::validation_with_code(err.detail.clone(), code.clone()),
                None => DomainError::validation(err.detail.clone()),
            },
            PluginErrorKind::Upstream => DomainError::DownstreamError(err.detail.clone()),
            PluginErrorKind::Unresolvable => DomainError::PluginNotFound(err.detail.clone()),
        }
    }
}

/// Context handed to request-phase hooks.
#[derive(Debug, Clone)]
pub struct RequestContext {
    /// Outbound request headers, mutable.
    pub headers: HeaderMap,
    /// Resolved proxy path (route path plus suffix).
    pub path: String,
    /// Resolved upstream alias.
    pub alias: String,
    /// Effective tenant of the caller.
    pub tenant_id: uuid::Uuid,
    /// Caller security context.
    pub security: Arc<SecurityContext>,
    /// Plugin-specific configuration from the binding.
    pub config: serde_json::Value,
    /// Shared ordering sink.
    pub trace: Trace,
}

impl RequestContext {
    /// Record a plugin hook invocation.
    pub fn record(&self, plugin_id: &str, phase: PluginPhase) {
        if let Ok(mut guard) = self.trace.lock() {
            guard.push(format!("{plugin_id}:{phase:?}"));
        }
    }
}

/// Context handed to response-phase hooks.
#[derive(Debug, Clone)]
pub struct ResponseContext {
    /// Upstream status code.
    pub status: StatusCode,
    /// Upstream response headers, mutable.
    pub headers: HeaderMap,
    /// Resolved upstream alias.
    pub alias: String,
    /// Effective tenant of the caller.
    pub tenant_id: uuid::Uuid,
    /// Plugin-specific configuration from the binding.
    pub config: serde_json::Value,
    /// Shared ordering sink.
    pub trace: Trace,
}

impl ResponseContext {
    /// Record a plugin hook invocation.
    pub fn record(&self, plugin_id: &str, phase: PluginPhase) {
        if let Ok(mut guard) = self.trace.lock() {
            guard.push(format!("{plugin_id}:{phase:?}"));
        }
    }
}

/// Context handed to error-phase hooks.
#[derive(Debug, Clone)]
pub struct ErrorContext {
    /// Status the gateway will return.
    pub status: StatusCode,
    /// Response headers of the error response.
    pub headers: HeaderMap,
    /// Resolved upstream alias.
    pub alias: String,
    /// The error that produced this response.
    pub error: DomainError,
    /// Plugin-specific configuration from the binding.
    pub config: serde_json::Value,
    /// Shared ordering sink.
    pub trace: Trace,
}

impl ErrorContext {
    /// Record a plugin hook invocation.
    pub fn record(&self, plugin_id: &str, phase: PluginPhase) {
        if let Ok(mut guard) = self.trace.lock() {
            guard.push(format!("{plugin_id}:{phase:?}"));
        }
    }
}

/// Credential injection hooks.
#[async_trait]
pub trait AuthPlugin: Send + Sync {
    /// Short identifier registered in the [`crate::infra::plugin::AuthPluginRegistry`].
    fn id(&self) -> &'static str;

    /// GTS base type of the plugin family.
    fn plugin_type(&self) -> &'static str {
        crate::domain::model::gts::AUTH_PLUGIN_TYPE
    }

    /// GTS identifier resolved by the registry.
    fn gts_id(&self) -> String {
        format!("{}cf.core.oagw.{}.v1", self.plugin_type(), self.id())
    }

    /// Inject credentials into the outbound request.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError`] when credentials cannot be resolved.
    async fn authenticate(&self, ctx: &mut RequestContext) -> Result<(), PluginError>;
}

/// Request and response validation hooks.
#[async_trait]
pub trait GuardPlugin: Send + Sync {
    /// Short identifier registered in the registry.
    fn id(&self) -> &'static str;

    /// GTS base type of the plugin family.
    fn plugin_type(&self) -> &'static str {
        crate::domain::model::gts::GUARD_PLUGIN_TYPE
    }

    /// GTS identifier resolved by the registry.
    fn gts_id(&self) -> String {
        format!("{}cf.core.oagw.{}.v1", self.plugin_type(), self.id())
    }

    /// Validate the outbound request.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError`] when the request must be rejected.
    async fn guard_request(&self, ctx: &RequestContext) -> Result<(), PluginError>;

    /// Validate the upstream response.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError`] when the response must be rejected.
    async fn guard_response(&self, ctx: &ResponseContext) -> Result<(), PluginError>;
}

/// Request, response and error mutation hooks.
#[async_trait]
pub trait TransformPlugin: Send + Sync {
    /// Short identifier registered in the registry.
    fn id(&self) -> &'static str;

    /// GTS base type of the plugin family.
    fn plugin_type(&self) -> &'static str {
        crate::domain::model::gts::TRANSFORM_PLUGIN_TYPE
    }

    /// GTS identifier resolved by the registry.
    fn gts_id(&self) -> String {
        format!("{}cf.core.oagw.{}.v1", self.plugin_type(), self.id())
    }

    /// Mutate the outbound request.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError`] when the transformation fails.
    async fn transform_request(&self, ctx: &mut RequestContext) -> Result<(), PluginError>;

    /// Mutate the upstream response.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError`] when the transformation fails.
    async fn transform_response(&self, ctx: &mut ResponseContext) -> Result<(), PluginError>;

    /// Mutate a gateway-generated error response.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError`] when the transformation fails.
    async fn transform_error(&self, ctx: &mut ErrorContext) -> Result<(), PluginError>;
}

/// The canonical GTS identifiers of the built-in plugins.
pub mod gts_helpers {
    /// No-op auth plugin.
    pub const NOOP_AUTH: &str = "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.noop.v1";
    /// API-key auth plugin.
    pub const APIKEY_AUTH: &str = "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1";
    /// `OAuth2` client credentials (form) auth plugin.
    pub const OAUTH2_CLIENT_CRED: &str =
        "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.oauth2_client_cred.v1";
    /// `OAuth2` client credentials (basic) auth plugin.
    pub const OAUTH2_CLIENT_CRED_BASIC: &str =
        "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.oauth2_client_cred_basic.v1";
    /// Required-headers guard plugin.
    pub const REQUIRED_HEADERS_GUARD: &str =
        "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1";
    /// Request-id transform plugin.
    pub const REQUEST_ID_TRANSFORM: &str =
        "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.request_id.v1";

    /// Catalog-only auth identifiers with no backing implementation.
    pub const CATALOG_ONLY_AUTH: [&str; 2] = [
        "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.basic.v1",
        "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.bearer.v1",
    ];

    /// Catalog-only guard identifiers with no backing implementation.
    pub const CATALOG_ONLY_GUARD: [&str; 2] = [
        "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.timeout.v1",
        "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.cors.v1",
    ];

    /// Catalog-only transform identifiers with no backing implementation.
    pub const CATALOG_ONLY_TRANSFORM: [&str; 2] = [
        "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.logging.v1",
        "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.metrics.v1",
    ];
}
