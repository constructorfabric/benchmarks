//! Plugin traits and the contexts they mutate (ADR-0002).
//!
//! Three traits with a deterministic execution order:
//! `Auth -> Guards -> Transform(on_request) -> upstream call ->
//! Transform(on_response | on_error)`.

use std::collections::BTreeMap;

use async_trait::async_trait;
use http::HeaderMap;
use serde_json::Value;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::domain::error::{ErrorKind, OagwError};

/// Failure raised from inside a plugin.
#[derive(Debug, Clone, thiserror::Error)]
pub enum PluginError {
    /// The plugin's `config` block is missing a key or malformed.
    #[error("plugin configuration error: {0}")]
    Config(String),
    /// Credential acquisition failed (bad credentials, IdP rejection).
    #[error("authentication failed: {0}")]
    Unauthenticated(String),
    /// A `cred://` reference did not resolve.
    #[error("secret not found: {0}")]
    SecretNotFound(String),
    /// The named plugin is not registered.
    #[error("plugin not found: {0}")]
    NotFound(String),
    /// Anything else — never carries secret material.
    #[error("plugin error: {0}")]
    Internal(String),
}

impl From<PluginError> for OagwError {
    fn from(err: PluginError) -> Self {
        let kind = match err {
            PluginError::Config(_) => ErrorKind::Validation,
            PluginError::Unauthenticated(_) => ErrorKind::AuthenticationFailed,
            PluginError::SecretNotFound(_) => ErrorKind::SecretNotFound,
            PluginError::NotFound(_) => ErrorKind::PluginNotFound,
            PluginError::Internal(_) => ErrorKind::Internal,
        };
        Self::new(kind, err.to_string())
    }
}

pub type PluginResult<T> = Result<T, PluginError>;

/// Mutable view of the outbound request handed to auth / guard / transform
/// plugins.
#[derive(Debug, Clone)]
pub struct RequestContext {
    pub security_context: SecurityContext,
    /// The binding's `config` block for the plugin currently executing.
    pub config: BTreeMap<String, Value>,
    pub method: String,
    /// Outbound path (already resolved from route + path suffix).
    pub path: String,
    /// Outbound query parameters, in order.
    pub query: Vec<(String, String)>,
    /// Outbound headers. Auth plugins inject credentials here.
    pub headers: HeaderMap,
    pub alias: String,
    pub upstream_id: Uuid,
    pub route_id: Option<Uuid>,
    pub request_id: Option<String>,
    /// Buffered request body, when the body was buffered.
    pub body: Option<bytes::Bytes>,
}

impl RequestContext {
    /// Read a config key as a string.
    #[must_use]
    pub fn config_str(&self, key: &str) -> Option<&str> {
        self.config.get(key).and_then(Value::as_str)
    }

    /// Read a required config key as a string.
    ///
    /// # Errors
    /// Returns [`PluginError::Config`] when the key is absent or not a string.
    pub fn require_config_str(&self, key: &str) -> PluginResult<&str> {
        self.config_str(key)
            .ok_or_else(|| PluginError::Config(format!("missing required config key '{key}'")))
    }
}

/// Mutable view of the response coming back from the upstream.
#[derive(Debug, Clone)]
pub struct ResponseContext {
    pub config: BTreeMap<String, Value>,
    pub status: u16,
    pub headers: HeaderMap,
    pub request_id: Option<String>,
}

impl ResponseContext {
    #[must_use]
    pub fn config_str(&self, key: &str) -> Option<&str> {
        self.config.get(key).and_then(Value::as_str)
    }
}

/// Error-phase context for `TransformPlugin::transform_error`.
#[derive(Debug, Clone)]
pub struct ErrorContext {
    pub config: BTreeMap<String, Value>,
    pub status: u16,
    pub error_type: String,
    pub detail: String,
}

/// Outcome of a guard phase.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GuardDecision {
    Allow,
    Reject {
        status: u16,
        error_code: String,
        message: String,
    },
}

impl GuardDecision {
    #[must_use]
    pub fn reject(status: u16, error_code: &str, message: impl Into<String>) -> Self {
        Self::Reject {
            status,
            error_code: error_code.to_owned(),
            message: message.into(),
        }
    }
}

/// Read-only view of the in-process plugin registries, used by the Control
/// Plane to reject bindings that can never resolve.
///
/// Catalog-only identifiers (`basic`, `bearer`, `timeout`, `cors`, `logging`,
/// `metrics`) are registered in the types-registry but answer `false` here —
/// they have no backing trait implementation.
pub trait PluginCatalog: Send + Sync {
    fn has_auth(&self, id: &str) -> bool;
    fn has_guard(&self, id: &str) -> bool;
    fn has_transform(&self, id: &str) -> bool;
}

/// Credential injection. One per upstream, executed before guards.
#[async_trait]
pub trait AuthPlugin: Send + Sync {
    fn id(&self) -> &str;
    fn plugin_type(&self) -> &str;
    /// # Errors
    /// Returns [`PluginError`] when credentials cannot be prepared.
    async fn authenticate(&self, ctx: &mut RequestContext) -> PluginResult<()>;
}

/// Validation / policy enforcement. Can reject before the upstream call.
#[async_trait]
pub trait GuardPlugin: Send + Sync {
    fn id(&self) -> &str;
    fn plugin_type(&self) -> &str;
    /// # Errors
    /// Returns [`PluginError`] on an internal guard failure (a *policy*
    /// rejection is `Ok(GuardDecision::Reject)`).
    async fn guard_request(&self, ctx: &RequestContext) -> PluginResult<GuardDecision>;
    /// # Errors
    /// Returns [`PluginError`] on an internal guard failure.
    async fn guard_response(&self, ctx: &ResponseContext) -> PluginResult<GuardDecision>;
}

/// Request / response / error mutation.
#[async_trait]
pub trait TransformPlugin: Send + Sync {
    fn id(&self) -> &str;
    fn plugin_type(&self) -> &str;
    /// Phases this plugin participates in (`on_request`, `on_response`, `on_error`).
    fn phases(&self) -> &[&str] {
        &["on_request", "on_response"]
    }
    /// # Errors
    /// Returns [`PluginError`] when the transformation fails.
    async fn transform_request(&self, _ctx: &mut RequestContext) -> PluginResult<()> {
        Ok(())
    }
    /// # Errors
    /// Returns [`PluginError`] when the transformation fails.
    async fn transform_response(&self, _ctx: &mut ResponseContext) -> PluginResult<()> {
        Ok(())
    }
    /// # Errors
    /// Returns [`PluginError`] when the transformation fails.
    async fn transform_error(&self, _ctx: &mut ErrorContext) -> PluginResult<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::error::ErrorKind;

    #[test]
    fn plugin_errors_map_onto_the_documented_statuses() {
        let cases = [
            (PluginError::Config("x".into()), ErrorKind::Validation),
            (
                PluginError::Unauthenticated("x".into()),
                ErrorKind::AuthenticationFailed,
            ),
            (
                PluginError::SecretNotFound("x".into()),
                ErrorKind::SecretNotFound,
            ),
            (PluginError::NotFound("x".into()), ErrorKind::PluginNotFound),
            (PluginError::Internal("x".into()), ErrorKind::Internal),
        ];
        for (err, expected) in cases {
            let mapped: OagwError = err.into();
            assert_eq!(mapped.kind, expected);
        }
    }

    #[test]
    fn guard_rejection_carries_the_error_code() {
        let d = GuardDecision::reject(400, "REQUIRED_HEADER_MISSING", "missing x");
        match d {
            GuardDecision::Reject {
                status, error_code, ..
            } => {
                assert_eq!(status, 400);
                assert_eq!(error_code, "REQUIRED_HEADER_MISSING");
            }
            GuardDecision::Allow => panic!("expected a rejection"),
        }
    }
}
