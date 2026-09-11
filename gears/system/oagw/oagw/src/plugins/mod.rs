//! Plugin traits, contexts and the built-in implementations.
//!
//! Three plugin types exist, each with its own trait ([`AuthPlugin`],
//! [`GuardPlugin`], [`TransformPlugin`]) and its own registry. The execution
//! order is Auth → Guards → Transforms(request) → upstream →
//! Transforms(response/error); upstream plugins run before route plugins.

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;
use toolkit_security::SecurityContext;

pub mod apikey_auth;
pub mod noop_auth;
pub mod oauth2_client_cred;
pub mod registries;
pub mod request_id_transform;
pub mod required_headers_guard;

pub use apikey_auth::ApiKeyAuthPlugin;
pub use noop_auth::NoopAuthPlugin;
pub use oauth2_client_cred::{CachedToken, OAuth2ClientCredAuthPlugin, OAuth2PluginConfig};
pub use registries::{AuthPluginRegistry, GuardPluginRegistry, TransformPluginRegistry};
pub use request_id_transform::{REQUEST_ID_HEADER, RequestIdTransformPlugin};
pub use required_headers_guard::{REQUIRED_HEADER_MISSING, RequiredHeadersGuardPlugin};

/// Failure surfaced by a plugin.
#[derive(Debug, Clone, thiserror::Error)]
pub enum PluginError {
    /// The plugin could not run (credential source down, misconfiguration).
    #[error("plugin internal error: {0}")]
    Internal(String),
    /// The plugin rejected the request.
    #[error("plugin rejected the request with {status}: {detail}")]
    Reject {
        /// HTTP status for the problem response.
        status: u16,
        /// OAGW error type identifier.
        type_id: &'static str,
        /// Human-readable explanation.
        detail: String,
    },
}

/// Decision returned by a guard.
#[derive(Debug, Clone)]
pub enum GuardDecision {
    /// Let the request continue.
    Continue,
    /// Reject the request with the given problem.
    Reject(crate::domain::error::OagwError),
}

impl GuardDecision {
    /// Whether the guard allowed the request.
    #[must_use]
    pub fn is_continue(&self) -> bool {
        matches!(self, Self::Continue)
    }
}

/// Mutable view of the outbound request a plugin may change.
#[derive(Debug, Clone)]
pub struct RequestContext {
    /// Caller identity propagated to plugins.
    pub security_context: SecurityContext,
    /// Headers about to be sent upstream.
    pub headers: http::HeaderMap,
    /// Plugin configuration from the binding.
    pub config: serde_json::Map<String, Value>,
}

/// View of the upstream response a plugin may inspect.
#[derive(Debug, Clone)]
pub struct ResponseContext {
    /// Upstream status.
    pub status: http::StatusCode,
    /// Upstream response headers.
    pub headers: http::HeaderMap,
    /// Plugin configuration from the binding.
    pub config: serde_json::Map<String, Value>,
}

/// View of a failed exchange a plugin may amend.
#[derive(Debug, Clone)]
pub struct ErrorContext {
    /// Status the gateway is about to return.
    pub status: http::StatusCode,
    /// Headers about to be returned.
    pub headers: http::HeaderMap,
    /// Problem detail.
    pub detail: String,
}

/// Injects authentication credentials into the outbound request.
#[async_trait]
pub trait AuthPlugin: Send + Sync {
    /// Plugin identifier used in logs and registries.
    fn id(&self) -> &str;
    /// GTS identifier of the plugin.
    fn plugin_type(&self) -> &str;

    /// Authenticates the outbound request.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError`] when credentials cannot be obtained or injected.
    async fn authenticate(&self, ctx: &mut RequestContext) -> Result<(), PluginError>;
}

/// Validates a request and may reject it.
#[async_trait]
pub trait GuardPlugin: Send + Sync {
    /// Plugin identifier.
    fn id(&self) -> &str;
    /// GTS identifier of the plugin.
    fn plugin_type(&self) -> &str;

    /// Validates the outbound request.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError::Internal`] for non-decision failures.
    async fn guard_request(&self, ctx: &RequestContext) -> Result<GuardDecision, PluginError>;

    /// Validates the upstream response.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError::Internal`] for non-decision failures.
    async fn guard_response(&self, ctx: &ResponseContext) -> Result<GuardDecision, PluginError>;
}

/// Modifies request/response/error data.
#[async_trait]
pub trait TransformPlugin: Send + Sync {
    /// Plugin identifier.
    fn id(&self) -> &str;
    /// GTS identifier of the plugin.
    fn plugin_type(&self) -> &str;

    /// Runs on the outbound request.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError`] when the transform cannot be applied.
    async fn transform_request(&self, ctx: &mut RequestContext) -> Result<(), PluginError>;

    /// Runs on the upstream response.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError`] when the transform cannot be applied.
    async fn transform_response(&self, ctx: &mut ResponseContext) -> Result<(), PluginError>;

    /// Runs on a gateway error before it is returned.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError`] when the transform cannot be applied.
    async fn transform_error(&self, ctx: &mut ErrorContext) -> Result<(), PluginError>;
}

/// Builds a `RequestContext` for a plugin invocation.
#[must_use]
pub fn request_context(
    security_context: &SecurityContext,
    headers: http::HeaderMap,
    config: &serde_json::Map<String, Value>,
) -> RequestContext {
    RequestContext {
        security_context: security_context.clone(),
        headers,
        config: config.clone(),
    }
}

/// Reads a string configuration key.
#[must_use]
pub fn config_string(config: &serde_json::Map<String, Value>, key: &str) -> Option<String> {
    config.get(key).and_then(Value::as_str).map(str::to_owned)
}

/// Reads a comma-separated configuration key into a list, trimming and dropping
/// blank entries.
#[must_use]
pub fn config_list(config: &serde_json::Map<String, Value>, key: &str) -> Vec<String> {
    match config.get(key) {
        Some(Value::String(raw)) => raw
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .collect(),
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .collect(),
        _ => Vec::new(),
    }
}

/// Sorts a plugin configuration deterministically and hashes it.
#[must_use]
pub fn hash_config(config: &serde_json::Map<String, Value>) -> u64 {
    let normalized: BTreeMap<String, String> = config
        .iter()
        .map(|(k, v)| (k.clone(), v.to_string()))
        .collect();
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    for (k, v) in &normalized {
        std::hash::Hash::hash(&k, &mut hasher);
        std::hash::Hash::hash(&v, &mut hasher);
    }
    std::hash::Hasher::finish(&hasher)
}

/// Type alias used by the registries.
pub type SharedAuthPlugin = Arc<dyn AuthPlugin>;
/// Type alias used by the registries.
pub type SharedGuardPlugin = Arc<dyn GuardPlugin>;
/// Type alias used by the registries.
pub type SharedTransformPlugin = Arc<dyn TransformPlugin>;
