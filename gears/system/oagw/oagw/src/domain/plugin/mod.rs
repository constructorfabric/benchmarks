//! Plugin contracts for the three OAGW plugin families.
//!
//! A plugin never sees raw credentials: an auth plugin *injects* them into the
//! outbound header map and returns nothing. Configurations are opaque
//! `serde_json::Value`s, validated by the plugin itself at registration time.

use std::collections::BTreeMap;

use async_trait::async_trait;
use uuid::Uuid;

use crate::domain::error::DomainError;

/// Everything a plugin knows about the request being executed.
#[derive(Debug, Clone, Default)]
pub struct PluginContext {
    /// Owning tenant.
    pub tenant_id: Uuid,
    /// Tenant ids of the caller's chain, descendant first, `tenant_id` first.
    ///
    /// Only configuration *lookups* walk it (an inherited custom plugin row
    /// lives in an ancestor's tenant); `tenant_id` stays the caller's own and
    /// is what credential lookups are scoped with.
    pub chain: Vec<Uuid>,
    /// Target upstream.
    pub upstream_id: Uuid,
    /// Matched route, when the call came from the data plane.
    pub route_id: Option<Uuid>,
    /// Routing alias in use.
    pub alias: String,
    /// Client IP, when known.
    pub client_ip: Option<std::net::IpAddr>,
    /// Authenticated downstream subject, when known.
    pub subject: Option<String>,
    /// Security context of the downstream caller, when the request was
    /// authenticated.
    ///
    /// Plugins that read a credential from the credstore must scope the lookup
    /// with *this* context — `tenant_id` above is the gateway tenant, not the
    /// caller, and credstore secrets are owned by (subject, tenant) pairs.
    /// System-initiated invocations leave it `None` and fall back to a
    /// tenant-derived context.
    pub security_context: Option<crate::domain::SecurityContext>,
    /// Free-form attributes accumulated by earlier plugins.
    pub attributes: BTreeMap<String, String>,
}

/// Request snapshot handed to guard / transform plugins.
#[derive(Debug, Clone, Default)]
pub struct PluginRequest {
    /// HTTP method (upper-cased).
    pub method: String,
    /// Request path, without the query string.
    pub path: String,
    /// Parsed query parameters.
    pub query: Vec<(String, String)>,
    /// Inbound headers (already stripped of hop-by-hop headers).
    pub headers: http::HeaderMap,
    /// Declared body size in bytes.
    pub body_len: usize,
}

/// Response snapshot handed to a guard plugin's response phase.
///
/// The body is never exposed: it keeps streaming untouched, which is what
/// keeps an SSE / long-poll hop safe.
#[derive(Debug, Clone, Default)]
pub struct PluginResponse {
    /// Upstream status code.
    pub status: u16,
    /// Upstream response headers (before the response header policy).
    pub headers: http::HeaderMap,
}

impl PluginResponse {
    /// Snapshot a response head.
    #[must_use]
    pub fn from_parts(status: http::StatusCode, headers: &http::HeaderMap) -> Self {
        Self {
            status: status.as_u16(),
            headers: headers.clone(),
        }
    }
}

/// Auth plugin: injects outbound credentials.
///
/// Implementations must place the credential directly in `headers` (or a
/// derived request attribute) and **must not** log or echo it.
#[async_trait]
pub trait AuthPlugin: Send + Sync {
    /// Stable short name of the built-in plugin (`apikey`, `oauth2_client_credentials`, …).
    fn name(&self) -> &'static str;

    /// Fully-qualified GTS id of the plugin.
    fn gts_id(&self) -> String {
        crate::domain::gts_helpers::auth_plugin(self.name())
    }

    /// Validate the `auth.config` object; called on the control plane.
    ///
    /// # Errors
    /// Returns [`DomainError::Validation`] when the configuration is unusable.
    fn validate_config(&self, config: &serde_json::Value) -> Result<(), DomainError>;

    /// Inject credentials into the outbound request.
    ///
    /// # Errors
    /// Returns [`DomainError::SecretNotFound`] when a referenced credential is
    /// missing and [`DomainError::AuthFailed`] when the credentials are
    /// structurally invalid.
    async fn authenticate(
        &self,
        ctx: &PluginContext,
        config: &serde_json::Value,
        headers: &mut http::HeaderMap,
    ) -> Result<(), DomainError>;
}

/// Guard plugin: validates an inbound request before it is proxied.
///
/// A guard may also police the upstream's *response* before it reaches the
/// caller (ADR-0009). The response phase is opt-in: the default
/// implementation is fail-open, so a plugin only overrides the half it cares
/// about.
#[async_trait]
pub trait GuardPlugin: Send + Sync {
    /// Stable short name of the built-in plugin.
    fn name(&self) -> &'static str;

    /// Fully-qualified GTS id of the plugin.
    fn gts_id(&self) -> String {
        crate::domain::gts_helpers::guard_plugin(self.name())
    }

    /// Validate the plugin configuration; called on the control plane.
    ///
    /// # Errors
    /// Returns [`DomainError::Validation`] when the configuration is unusable.
    fn validate_config(&self, config: &serde_json::Value) -> Result<(), DomainError>;

    /// Enforce the guard on the inbound request.
    ///
    /// # Errors
    /// Returns the domain error the client should see (typically
    /// [`DomainError::RequiredHeaderMissing`]).
    async fn guard(
        &self,
        ctx: &PluginContext,
        config: &serde_json::Value,
        request: &PluginRequest,
    ) -> Result<(), DomainError>;

    /// Enforce the guard on the upstream response before it is returned.
    ///
    /// # Errors
    /// Returns the domain error the client should see — a *gateway* error,
    /// typically [`DomainError::ResponseHeaderMissing`] (502).
    async fn guard_response(
        &self,
        _ctx: &PluginContext,
        _config: &serde_json::Value,
        _response: &PluginResponse,
    ) -> Result<(), DomainError> {
        Ok(())
    }
}


/// Transform plugin: rewrites the outbound request (and, later, the response).
#[async_trait]
pub trait TransformPlugin: Send + Sync {
    /// Stable short name of the built-in plugin.
    fn name(&self) -> &'static str;

    /// Fully-qualified GTS id of the plugin.
    fn gts_id(&self) -> String {
        crate::domain::gts_helpers::transform_plugin(self.name())
    }

    /// Validate the plugin configuration; called on the control plane.
    ///
    /// # Errors
    /// Returns [`DomainError::Validation`] when the configuration is unusable.
    fn validate_config(&self, config: &serde_json::Value) -> Result<(), DomainError>;

    /// Rewrite the outbound request.
    ///
    /// Implementations may record values in `ctx.attributes` so the response
    /// half of the same plugin can pick them up.
    async fn transform_request(
        &self,
        ctx: &mut PluginContext,
        config: &serde_json::Value,
        request: &mut http::Request<()>,
    );

    /// Rewrite the upstream response headers before they reach the client.
    ///
    /// The body is never handed to a transform plugin: it stays streaming. The
    /// default implementation is a no-op, so a plugin only needs to override
    /// the half it cares about.
    async fn transform_response(
        &self,
        _ctx: &mut PluginContext,
        _config: &serde_json::Value,
        _response: &mut http::Response<()>,
    ) {}
}

/// Registry of built-in plugins, keyed by short name.
#[derive(Default)]
pub struct PluginRegistry {
    auth: Vec<std::sync::Arc<dyn AuthPlugin>>,
    guard: Vec<std::sync::Arc<dyn GuardPlugin>>,
    transform: Vec<std::sync::Arc<dyn TransformPlugin>>,
}

impl std::fmt::Debug for PluginRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PluginRegistry")
            .field("auth", &self.auth.iter().map(|p| p.name()).collect::<Vec<_>>())
            .field(
                "guard",
                &self.guard.iter().map(|p| p.name()).collect::<Vec<_>>(),
            )
            .field(
                "transform",
                &self.transform.iter().map(|p| p.name()).collect::<Vec<_>>(),
            )
            .finish()
    }
}

impl PluginRegistry {
    /// Build an empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Register an auth plugin.
    pub fn register_auth(&mut self, plugin: std::sync::Arc<dyn AuthPlugin>) {
        self.auth.push(plugin);
    }

    /// Register a guard plugin.
    pub fn register_guard(&mut self, plugin: std::sync::Arc<dyn GuardPlugin>) {
        self.guard.push(plugin);
    }

    /// Register a transform plugin.
    pub fn register_transform(&mut self, plugin: std::sync::Arc<dyn TransformPlugin>) {
        self.transform.push(plugin);
    }

    /// Look up an auth plugin by GTS id or short name.
    #[must_use]
    pub fn auth(&self, reference: &str) -> Option<std::sync::Arc<dyn AuthPlugin>> {
        let short = crate::domain::dto::builtin_plugin_name(reference);
        self.auth
            .iter()
            .find(|p| Some(p.name()) == short.as_deref() || p.gts_id() == reference)
            .cloned()
    }

    /// Look up a guard plugin by GTS id or short name.
    #[must_use]
    pub fn guard(&self, reference: &str) -> Option<std::sync::Arc<dyn GuardPlugin>> {
        let short = crate::domain::dto::builtin_plugin_name(reference);
        self.guard
            .iter()
            .find(|p| Some(p.name()) == short.as_deref() || p.gts_id() == reference)
            .cloned()
    }

    /// Look up a transform plugin by GTS id or short name.
    #[must_use]
    pub fn transform(&self, reference: &str) -> Option<std::sync::Arc<dyn TransformPlugin>> {
        let short = crate::domain::dto::builtin_plugin_name(reference);
        self.transform
            .iter()
            .find(|p| Some(p.name()) == short.as_deref() || p.gts_id() == reference)
            .cloned()
    }

    /// All registered auth plugin ids.
    #[must_use]
    pub fn auth_ids(&self) -> Vec<String> {
        self.auth.iter().map(|p| p.gts_id()).collect()
    }

    /// All registered guard plugin ids.
    #[must_use]
    pub fn guard_ids(&self) -> Vec<String> {
        self.guard.iter().map(|p| p.gts_id()).collect()
    }

    /// All registered transform plugin ids.
    #[must_use]
    pub fn transform_ids(&self) -> Vec<String> {
        self.transform.iter().map(|p| p.gts_id()).collect()
    }
}
