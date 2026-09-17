//! Plugin SPI (`docs/ADR/0002-plugin-system.md`).
//!
//! Three traits, one per plugin purpose, exactly as the ADR spells them:
//!
//! | Trait | Phase | Can reject |
//! |---|---|---|
//! | [`AuthPlugin`] | credential injection, before guards | yes |
//! | [`GuardPlugin`] | policy validation, before transforms | yes |
//! | [`TransformPlugin`] | request/response/error rewriting | no |
//!
//! The traits are the same for built-in and external plugins: a plugin is
//! identified by its GTS type id ([`AuthPlugin::plugin_type`]) and registered
//! in a registry keyed by that id, so an upstream's `auth.plugin_type` and a
//! route's `plugin_ref` resolve to the same object regardless of where the
//! implementation lives.
//!
//! Nothing in this module depends on a transport: the contexts carry the
//! canonical `http::HeaderMap` and `serde_json::Value` only. The built-in
//! implementations live in `crate::infra::plugin`.
use async_trait::async_trait;

pub mod context;
pub mod errors;
pub mod secret;

pub use context::{
    AuthContext, ErrorContext, GuardDecision, PluginAttributes, PluginError,
    REQUIRED_HEADER_MISSING, RequestContext, ResponseContext,
};
pub use errors::{
    AUTH_FAILED, CONNECTION_TIMEOUT, CORS_METHOD_NOT_ALLOWED, CORS_ORIGIN_NOT_ALLOWED,
    DOWNSTREAM_ERROR, INTERNAL, INVALID_TARGET_HOST, LINK_UNAVAILABLE, MISSING_TARGET_HOST,
    PAYLOAD_TOO_LARGE, PLUGIN_NOT_FOUND, PROTOCOL_ERROR, RATE_LIMIT_EXCEEDED, REQUEST_TIMEOUT,
    ROUTE_NOT_FOUND, SECRET_NOT_FOUND, STREAM_ABORTED, UNKNOWN_TARGET_HOST, VALIDATION,
    problem_type,
};
pub use secret::{ResolvedSecret, SecretError, SecretResolver, strip_cred_scheme};

/// Injects authentication credentials into the outbound request.
///
/// Executed once per request, before guards. A plugin may only reject with a
/// problem-document-shaped [`PluginError`]; it must not fail silently.
#[async_trait]
pub trait AuthPlugin: Send + Sync {
    /// Short registry name (`apikey`, `oauth2_client_cred`).
    fn id(&self) -> &str;
    /// Full GTS plugin type id the configuration binds against.
    fn plugin_type(&self) -> &str;
    /// Inject credentials. `ctx.config` is the plugin's effective
    /// configuration; `ctx.headers` is the outbound header set to mutate.
    ///
    /// # Errors
    ///
    /// [`PluginError`] when credentials could not be produced or the plugin
    /// rejects the request.
    async fn authenticate(&self, ctx: &mut RequestContext) -> Result<(), PluginError>;
}

/// Validates a request (or an upstream response) and can reject the hop.
#[async_trait]
pub trait GuardPlugin: Send + Sync {
    /// Short registry name.
    fn id(&self) -> &str;
    /// Full GTS plugin type id the configuration binds.
    fn plugin_type(&self) -> &str;
    /// Validate the request before it is transformed.
    ///
    /// # Errors
    ///
    /// [`PluginError`] when the guard itself fails; the returned
    /// [`GuardDecision`] carries the rejection.
    async fn guard_request(&self, ctx: &RequestContext) -> Result<GuardDecision, PluginError> {
        let _ = ctx;
        Ok(GuardDecision::allow())
    }
    /// Validate the upstream response before it is returned to the caller.
    ///
    /// # Errors
    ///
    /// [`PluginError`] when the guard itself fails; the returned
    /// [`GuardDecision`] carries the rejection.
    async fn guard_response(&self, ctx: &ResponseContext) -> Result<GuardDecision, PluginError> {
        let _ = ctx;
        Ok(GuardDecision::allow())
    }
}

/// Rewrites the request, the upstream response, or the failure the gateway
/// is about to report.
#[async_trait]
pub trait TransformPlugin: Send + Sync {
    /// Short registry name.
    fn id(&self) -> &str;
    /// Full GTS plugin type id the configuration binds.
    fn plugin_type(&self) -> &str;
    /// Modify the outbound request.
    ///
    /// # Errors
    ///
    /// [`PluginError`] when the transformation cannot be applied.
    async fn transform_request(&self, ctx: &mut RequestContext) -> Result<(), PluginError> {
        let _ = ctx;
        Ok(())
    }
    /// Modify the upstream response before it is forwarded.
    ///
    /// # Errors
    ///
    /// [`PluginError`] when the transformation cannot be applied.
    async fn transform_response(&self, ctx: &mut ResponseContext) -> Result<(), PluginError> {
        let _ = ctx;
        Ok(())
    }
    /// Modify the failure the gateway is about to render.
    ///
    /// # Errors
    ///
    /// [`PluginError`] when the transformation cannot be applied.
    async fn transform_error(&self, ctx: &mut ErrorContext) -> Result<(), PluginError> {
        let _ = ctx;
        Ok(())
    }
}
