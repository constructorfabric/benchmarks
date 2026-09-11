//! Plugin traits and request/response contexts (DESIGN §3.2 "Plugin System",
//! ADR 0008, ADR 0009).
//!
//! Three separate traits with a deterministic execution order:
//! Auth → Guards → Transform(request) → upstream call → Transform(response/error).
//! Upstream-bound plugins always run before route-bound ones.

use std::collections::BTreeMap;

use async_trait::async_trait;
use http::HeaderMap;

use crate::domain::error::DomainError;

/// Cross-plugin, cross-phase state.
#[derive(Debug, Default, Clone)]
pub struct PluginAttributes {
    inner: BTreeMap<String, String>,
}

impl PluginAttributes {
    /// Stores a value for later phases.
    pub fn set(&mut self, key: impl Into<String>, value: impl Into<String>) {
        self.inner.insert(key.into(), value.into());
    }

    /// Reads a value set by an earlier phase.
    #[must_use]
    pub fn get(&self, key: &str) -> Option<&str> {
        self.inner.get(key).map(String::as_str)
    }
}

/// Mutable view over the outbound request, shared by all three plugin types.
#[derive(Debug)]
pub struct RequestContext {
    /// Calling tenant.
    pub tenant_id: String,
    /// The proxy alias addressed.
    pub alias: String,
    /// The path that will be appended to the route prefix.
    pub path: String,
    /// Query parameters that survived the allowlist.
    pub query: Vec<(String, String)>,
    /// Outbound request headers (already stripped of routing + hop-by-hop).
    pub headers: HeaderMap,
    /// Cross-plugin state.
    pub attributes: PluginAttributes,
}

impl RequestContext {
    /// Builds a request context.
    #[must_use]
    pub fn new(
        tenant_id: impl Into<String>,
        alias: impl Into<String>,
        path: impl Into<String>,
        query: Vec<(String, String)>,
        headers: HeaderMap,
    ) -> Self {
        Self {
            tenant_id: tenant_id.into(),
            alias: alias.into(),
            path: path.into(),
            query,
            headers,
            attributes: PluginAttributes::default(),
        }
    }

    /// Rebuilds the query string from the surviving parameters.
    #[must_use]
    pub fn query_string(&self) -> String {
        form_urlencoded::Serializer::new(String::new())
            .extend_pairs(self.query.iter().map(|(k, v)| (k.as_str(), v.as_str())))
            .finish()
    }
}

/// Mutable view over the upstream response, before it is returned to the client.
#[derive(Debug)]
pub struct ResponseContext<'a> {
    /// Upstream status.
    pub status: http::StatusCode,
    /// Upstream response headers (mutable).
    pub headers: HeaderMap,
    /// The request that produced this response.
    pub request: &'a RequestContext,
}

/// Mutable view over a gateway-generated error response.
#[derive(Debug)]
pub struct ErrorContext<'a> {
    /// Status the gateway will answer with.
    pub status: http::StatusCode,
    /// Headers to attach to the error response (mutable).
    pub headers: HeaderMap,
    /// The request that was rejected.
    pub request: &'a RequestContext,
    /// The gateway error about to be returned.
    pub error: &'a DomainError,
}

/// Credential injection. Exactly one per upstream.
#[async_trait]
pub trait AuthPlugin: Send + Sync {
    /// The plugin's GTS identifier.
    fn id(&self) -> &'static str;

    /// Injects outbound credentials into `ctx`.
    ///
    /// # Errors
    /// Returns the error the proxy should surface (`401`/`500`).
    async fn authenticate(
        &self,
        ctx: &mut RequestContext,
        config: &PluginConfig,
    ) -> Result<(), DomainError>;
}

/// Validation that can reject a request or a response.
#[async_trait]
pub trait GuardPlugin: Send + Sync {
    /// The plugin's GTS identifier.
    fn id(&self) -> &'static str;

    /// Validates the outbound request.
    ///
    /// # Errors
    /// Returning an error rejects the request before it reaches the upstream.
    async fn guard_request(
        &self,
        ctx: &mut RequestContext,
        config: &PluginConfig,
    ) -> Result<(), DomainError> {
        let _ = (ctx, config);
        Ok(())
    }

    /// Validates the upstream response.
    ///
    /// # Errors
    /// Returns an error to reject the response (`502`).
    async fn guard_response(
        &self,
        ctx: &mut ResponseContext<'_>,
        config: &PluginConfig,
    ) -> Result<(), DomainError> {
        let _ = (ctx, config);
        Ok(())
    }
}

/// Request/response/error mutation.
#[async_trait]
pub trait TransformPlugin: Send + Sync {
    /// The plugin's GTS identifier.
    fn id(&self) -> &'static str;

    /// Mutates the outbound request.
    ///
    /// # Errors
    /// Returns an error to reject the request.
    async fn on_request(
        &self,
        ctx: &mut RequestContext,
        config: &PluginConfig,
    ) -> Result<(), DomainError> {
        let _ = (ctx, config);
        Ok(())
    }

    /// Mutates the response before it is returned.
    ///
    /// # Errors
    /// Returns an error to reject the response (`502`).
    async fn on_response(
        &self,
        ctx: &mut ResponseContext<'_>,
        config: &PluginConfig,
    ) -> Result<(), DomainError> {
        let _ = (ctx, config);
        Ok(())
    }

    /// Mutates a gateway-generated error response.
    ///
    /// # Errors
    /// Returns an error to fall back to the default error body.
    async fn on_error(
        &self,
        ctx: &mut ErrorContext<'_>,
        config: &PluginConfig,
    ) -> Result<(), DomainError> {
        let _ = (ctx, config);
        Ok(())
    }
}

/// Resolved plugin configuration: the identifier plus the bind-time config.
#[derive(Debug, Clone)]
pub struct PluginConfig {
    /// The plugin's GTS identifier (or custom-plugin UUID).
    pub plugin_ref: String,
    /// Bind-time configuration keys.
    pub values: PluginConfigMap,
}

/// Plugin configuration map, as supplied in `auth.config` /
/// `plugins.items[].config`.
pub type PluginConfigMap = BTreeMap<String, serde_json::Value>;

impl PluginConfig {
    /// Builds a plugin config from a binding.
    #[must_use]
    pub fn from_binding(plugin_ref: &str, values: Option<&PluginConfigMap>) -> Self {
        Self {
            plugin_ref: plugin_ref.to_owned(),
            values: values.cloned().unwrap_or_default(),
        }
    }

    /// Reads a string config key.
    #[must_use]
    pub fn string(&self, key: &str) -> Option<&str> {
        self.values.get(key).and_then(serde_json::Value::as_str)
    }
}

impl From<&crate::domain::model::PluginBinding> for PluginConfig {
    fn from(binding: &crate::domain::model::PluginBinding) -> Self {
        Self::from_binding(binding.plugin_ref(), binding.config())
    }
}
