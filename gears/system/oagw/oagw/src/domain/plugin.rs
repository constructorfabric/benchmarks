//! The three plugin traits and the contexts they operate on
//! (`cpt-cf-oagw-adr-plugin-system`).
//!
//! Built-in and external plugins implement the same traits — there is no
//! special-casing — and the Data Plane always runs them in the order
//! `Auth → Guards → Transform(request) → upstream → Transform(response|error)`,
//! with upstream-level bindings ahead of route-level ones.

use async_trait::async_trait;
use axum::http::{HeaderMap, Method, StatusCode};
use bytes::Bytes;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use super::error::{ErrorKind, OagwError};
use super::model::PluginConfig;

/// Failure raised by a plugin.
#[derive(Debug, Clone, thiserror::Error)]
pub enum PluginError {
    /// Credential preparation failed — surfaces as `401`.
    #[error("authentication failed: {0}")]
    Unauthenticated(String),
    /// A referenced secret could not be resolved — surfaces as `500`.
    #[error("secret not found: {0}")]
    SecretNotFound(String),
    /// The plugin identifier could not be resolved — surfaces as `503`.
    #[error("plugin not found: {0}")]
    NotFound(String),
    /// Plugin configuration is invalid — surfaces as `400`.
    #[error("invalid plugin configuration: {0}")]
    InvalidConfig(String),
    /// Anything else — surfaces as `500`.
    #[error("plugin failure: {0}")]
    Internal(String),
}

impl From<PluginError> for OagwError {
    fn from(err: PluginError) -> Self {
        let kind = match &err {
            PluginError::Unauthenticated(_) => ErrorKind::AuthenticationFailed,
            PluginError::SecretNotFound(_) => ErrorKind::SecretNotFound,
            PluginError::NotFound(_) => ErrorKind::PluginNotFound,
            PluginError::InvalidConfig(_) => ErrorKind::Validation,
            PluginError::Internal(_) => ErrorKind::Internal,
        };
        Self::new(kind, err.to_string())
    }
}

/// What a guard decided about a request or response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GuardDecision {
    /// Continue the chain.
    Allow,
    /// Stop and answer the client.
    Reject {
        /// HTTP status to answer with.
        status: u16,
        /// Machine-readable rejection code.
        error_code: String,
        /// Human-readable explanation.
        message: String,
    },
}

impl GuardDecision {
    /// Reject with a status, code and message.
    pub fn reject(status: u16, error_code: &str, message: impl Into<String>) -> Self {
        Self::Reject {
            status,
            error_code: error_code.to_owned(),
            message: message.into(),
        }
    }
}

/// Everything an auth plugin needs to mint and inject a credential.
#[derive(Debug)]
pub struct AuthContext {
    /// Caller identity — the cache-key and CredStore-access boundary.
    pub security_context: SecurityContext,
    /// The binding's configuration.
    pub config: PluginConfig,
    /// Outbound headers; the plugin injects into these.
    pub headers: HeaderMap,
    /// Outbound query parameters; API-key plugins may inject here.
    pub query: Vec<(String, String)>,
    /// Alias of the upstream being called (diagnostics only).
    pub upstream_alias: String,
}

/// The outbound request as guards and transforms see it.
#[derive(Debug)]
pub struct RequestContext {
    /// Caller identity.
    pub security_context: SecurityContext,
    /// Outbound method.
    pub method: Method,
    /// Outbound path.
    pub path: String,
    /// Outbound query parameters.
    pub query: Vec<(String, String)>,
    /// Outbound headers.
    pub headers: HeaderMap,
    /// Buffered request body.
    pub body: Bytes,
    /// The binding's configuration.
    pub config: PluginConfig,
    /// Alias of the upstream being called.
    pub upstream_alias: String,
    /// Identifier of the upstream being called.
    pub upstream_id: Uuid,
}

impl RequestContext {
    /// Add or overwrite a query parameter.
    pub fn set_query(&mut self, key: &str, value: &str) {
        self.query.retain(|(k, _)| k != key);
        self.query.push((key.to_owned(), value.to_owned()));
    }
}

/// The upstream response as guards and transforms see it.
#[derive(Debug)]
pub struct ResponseContext {
    /// Upstream status.
    pub status: StatusCode,
    /// Upstream headers (already stripped of hop-by-hop names).
    pub headers: HeaderMap,
    /// The binding's configuration.
    pub config: PluginConfig,
    /// Headers of the request that produced this response.
    pub request_headers: HeaderMap,
}

/// A gateway-side failure as `transform_error` sees it.
#[derive(Debug)]
pub struct ErrorContext {
    /// Rendered error.
    pub message: String,
    /// The binding's configuration.
    pub config: PluginConfig,
}

/// What the Control Plane needs to know to validate a plugin binding at
/// write time: whether a named identifier resolves at all, and to which kind.
///
/// This is deliberately narrower than the registries themselves — validating
/// a binding must not require the ability to *execute* a plugin.
pub trait PluginCatalog: Send + Sync {
    /// Whether an auth plugin identifier resolves.
    fn has_auth(&self, plugin_type: &str) -> bool;
    /// Whether a guard plugin identifier resolves and is bindable.
    fn has_guard(&self, plugin_type: &str) -> bool;
    /// Whether a transform plugin identifier resolves.
    fn has_transform(&self, plugin_type: &str) -> bool;
    /// Resolvable auth plugin identifiers, for error messages.
    fn auth_ids(&self) -> Vec<String>;
}

/// Credential injection. One per upstream, executed before guards.
#[async_trait]
pub trait AuthPlugin: Send + Sync {
    /// Short plugin name.
    fn id(&self) -> &str;
    /// Full GTS plugin identifier.
    fn plugin_type(&self) -> &str;
    /// Inject credentials into `ctx`.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError`] when the credential cannot be prepared.
    async fn authenticate(&self, ctx: &mut AuthContext) -> Result<(), PluginError>;
}

/// Validation / policy enforcement. May reject.
#[async_trait]
pub trait GuardPlugin: Send + Sync {
    /// Short plugin name.
    fn id(&self) -> &str;
    /// Full GTS plugin identifier.
    fn plugin_type(&self) -> &str;
    /// Inspect the outbound request.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError`] when the guard itself fails.
    async fn guard_request(&self, ctx: &RequestContext) -> Result<GuardDecision, PluginError>;
    /// Inspect the upstream response.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError`] when the guard itself fails.
    async fn guard_response(&self, ctx: &ResponseContext) -> Result<GuardDecision, PluginError>;
}

/// Request / response mutation.
#[async_trait]
pub trait TransformPlugin: Send + Sync {
    /// Short plugin name.
    fn id(&self) -> &str;
    /// Full GTS plugin identifier.
    fn plugin_type(&self) -> &str;
    /// Phases this plugin participates in.
    fn phases(&self) -> &[&str] {
        &["on_request", "on_response"]
    }
    /// Mutate the outbound request.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError`] on failure.
    async fn transform_request(&self, ctx: &mut RequestContext) -> Result<(), PluginError> {
        let _ = ctx;
        Ok(())
    }
    /// Mutate the response on its way back to the client.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError`] on failure.
    async fn transform_response(&self, ctx: &mut ResponseContext) -> Result<(), PluginError> {
        let _ = ctx;
        Ok(())
    }
    /// Observe a gateway-side failure.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError`] on failure.
    async fn transform_error(&self, ctx: &mut ErrorContext) -> Result<(), PluginError> {
        let _ = ctx;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plugin_errors_map_to_the_documented_statuses() {
        assert_eq!(
            OagwError::from(PluginError::Unauthenticated("x".into())).status(),
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            OagwError::from(PluginError::SecretNotFound("x".into())).status(),
            StatusCode::INTERNAL_SERVER_ERROR
        );
        assert_eq!(
            OagwError::from(PluginError::NotFound("x".into())).status(),
            StatusCode::SERVICE_UNAVAILABLE
        );
        assert_eq!(
            OagwError::from(PluginError::InvalidConfig("x".into())).status(),
            StatusCode::BAD_REQUEST
        );
    }

    #[test]
    fn set_query_overwrites_rather_than_duplicates() {
        let mut ctx = RequestContext {
            security_context: SecurityContext::anonymous(),
            method: Method::GET,
            path: "/v1/chat".to_owned(),
            query: vec![("model".to_owned(), "gpt-4".to_owned())],
            headers: HeaderMap::new(),
            body: Bytes::new(),
            config: crate::domain::model::PluginConfig::new(),
            upstream_alias: "api.openai.com".to_owned(),
            upstream_id: Uuid::nil(),
        };
        ctx.set_query("api_version", "2024-01");
        ctx.set_query("model", "gpt-4o");
        assert_eq!(
            ctx.query,
            vec![
                ("api_version".to_owned(), "2024-01".to_owned()),
                ("model".to_owned(), "gpt-4o".to_owned()),
            ]
        );
    }

    #[test]
    fn guard_rejection_carries_its_code() {
        let decision = GuardDecision::reject(400, "REQUIRED_HEADER_MISSING", "missing x");
        match decision {
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
