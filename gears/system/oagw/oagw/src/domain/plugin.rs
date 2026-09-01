//! Plugin traits (ADR-0002) and shared plugin context types.
//!
//! Execution order for a proxied request: auth -> guards (request) ->
//! transforms (request) -> upstream call -> transforms (response/error).

use std::collections::BTreeMap;

use async_trait::async_trait;
use thiserror::Error;
use toolkit_security::SecurityContext;
use uuid::Uuid;

/// Stable, machine-readable error codes surfaced on 400 / 502 guard rejections.
pub const CODE_REQUIRED_HEADER_MISSING: &str = "REQUIRED_HEADER_MISSING";

/// Failure of a plugin execution.
#[derive(Debug, Error)]
pub enum PluginError {
    /// A guard rejected the request/response.
    #[error("{code}: {detail}")]
    Reject {
        /// HTTP status to associate with the rejection.
        status: u16,
        /// Stable machine-readable code.
        code: &'static str,
        /// Human-readable detail.
        detail: String,
    },
    /// An auth plugin could not authenticate against the upstream.
    #[error("auth failed: {detail}")]
    AuthFailed {
        /// Failure detail (no secret material).
        detail: String,
    },
    /// The plugin configuration is invalid.
    #[error("plugin config: {detail}")]
    Config {
        /// Configuration problem detail.
        detail: String,
    },
    /// A referenced secret could not be resolved.
    #[error("credstore: secret {reference} not found")]
    Secret {
        /// The `cred://` reference (never the material).
        reference: String,
    },
    /// A referenced plugin is not registered.
    #[error("plugin {plugin_ref} not found")]
    NotFound {
        /// The unresolved plugin reference.
        plugin_ref: String,
    },
    /// Internal plugin failure.
    #[error("plugin internal: {diagnostic}")]
    Internal {
        /// Redacted diagnostic.
        diagnostic: String,
    },
}

impl PluginError {
    /// Convenience constructor for a reject error.
    #[must_use]
    pub fn reject(status: u16, code: &'static str, detail: impl Into<String>) -> Self {
        Self::Reject {
            status,
            code,
            detail: detail.into(),
        }
    }
}

/// Request-phase context handed to plugins.
pub struct RequestCtx<'a> {
    /// Gateway authentication context.
    pub security: &'a SecurityContext,
    /// Plugin configuration (from the binding).
    pub config: &'a BTreeMap<String, serde_json::Value>,
    /// Mutable request headers (plugins may add/remove).
    pub headers: &'a mut http::HeaderMap,
    /// Subject tenant (alias for `security.subject_tenant_id()`).
    pub tenant_id: Uuid,
}

/// Response-phase context handed to plugins.
pub struct ResponseCtx<'a> {
    /// Gateway authentication context.
    pub security: &'a SecurityContext,
    /// Plugin configuration (from the binding).
    pub config: &'a BTreeMap<String, serde_json::Value>,
    /// Mutable response headers (plugins may add/remove).
    pub headers: &'a mut http::HeaderMap,
    /// Upstream response status.
    pub status: http::StatusCode,
}

/// Credential-injection plugin. One per upstream.
#[async_trait]
pub trait AuthPlugin: Send + Sync {
    /// GTS identifier this plugin is registered under.
    fn id(&self) -> &str;

    /// Inject credentials into the outbound request headers.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError::AuthFailed`] when credentials cannot be
    /// obtained, [`PluginError::Secret`] when a `cred://` reference is
    /// unresolvable, and [`PluginError::Config`] for malformed config.
    async fn authenticate(&self, ctx: &mut RequestCtx<'_>) -> Result<(), PluginError>;
}

/// Validation / policy-enforcement plugin. Zero or more per upstream/route.
#[async_trait]
pub trait GuardPlugin: Send + Sync {
    /// GTS identifier this plugin is registered under.
    fn id(&self) -> &str;

    /// Validate the request before forwarding; reject via
    /// [`PluginError::Reject`].
    ///
    /// # Errors
    ///
    /// Returns [`PluginError::Reject`] when a request-phase rule fails.
    async fn guard_request(&self, ctx: &mut RequestCtx<'_>) -> Result<(), PluginError>;

    /// Validate the upstream response before returning it; reject via
    /// [`PluginError::Reject`].
    ///
    /// # Errors
    ///
    /// Returns [`PluginError::Reject`] when a response-phase rule fails.
    /// Response-phase rejections map to 502 by contract.
    async fn guard_response(&self, ctx: &mut ResponseCtx<'_>) -> Result<(), PluginError>;
}

/// Request/response mutation plugin. Zero or more per upstream/route.
#[async_trait]
pub trait TransformPlugin: Send + Sync {
    /// GTS identifier this plugin is registered under.
    fn id(&self) -> &str;

    /// Mutate the outbound request before it is forwarded.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError::Internal`] on unexpected failures.
    async fn on_request(&self, ctx: &mut RequestCtx<'_>) -> Result<(), PluginError>;

    /// Mutate the response before it is returned to the caller.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError::Internal`] on unexpected failures.
    async fn on_response(&self, ctx: &mut ResponseCtx<'_>) -> Result<(), PluginError>;

    /// Called when the upstream call failed after request transforms ran
    /// (error-hook; default no-op).
    ///
    /// # Errors
    ///
    /// Returns [`PluginError::Internal`] on unexpected failures.
    async fn on_error(&self, _ctx: &RequestCtx<'_>) -> Result<(), PluginError> {
        Ok(())
    }
}
