//! Plugin traits and their context types, exactly as ADR 0002 states them.
//!
//! Three plugin types exist: [`AuthPlugin`] (credential injection, one per
//! upstream), [`GuardPlugin`] (validation, can reject, many) and
//! [`TransformPlugin`] (request/response mutation, many). Upstream plugins
//! execute before route plugins.

use std::collections::BTreeMap;

use async_trait::async_trait;

use crate::domain::error::DomainError;

/// Outcome of a guard evaluation.
#[derive(Debug, Clone)]
pub enum GuardDecision {
    /// Continue the chain.
    Next,
    /// Stop the chain and reject the request with the given domain error.
    Reject(DomainError),
}

impl GuardDecision {
    /// `true` when the chain may continue.
    pub fn is_next(&self) -> bool {
        matches!(self, GuardDecision::Next)
    }

    /// Shorthand for [`GuardDecision::Reject`].
    pub fn reject(err: DomainError) -> Self {
        GuardDecision::Reject(err)
    }
}

/// Error returned by a plugin hook.
#[derive(Debug, Clone, thiserror::Error)]
pub enum PluginError {
    /// The plugin failed its own validation and wants the request rejected.
    #[error("{0}")]
    Reject(#[from] DomainError),
    /// The plugin itself failed.
    #[error("plugin `{plugin_id}` failed: {message}")]
    Failure {
        plugin_id: String,
        message: String,
    },
}

impl PluginError {
    /// Builds a [`PluginError::Failure`].
    pub fn failure(plugin_id: impl Into<String>, message: impl Into<String>) -> Self {
        PluginError::Failure { plugin_id: plugin_id.into(), message: message.into() }
    }
}

impl From<PluginError> for DomainError {
    fn from(e: PluginError) -> Self {
        match e {
            PluginError::Reject(d) => d,
            PluginError::Failure { plugin_id, message } => DomainError::Internal {
                diagnostic: format!("plugin `{plugin_id}` failed: {message}"),
            },
        }
    }
}

/// Mutable view of the outbound request handed to plugins.
#[derive(Debug, Clone, Default)]
pub struct RequestContext {
    /// Resolved upstream id (GTS instance).
    pub upstream_id: Option<String>,
    /// Resolved upstream alias.
    pub alias: Option<String>,
    /// Request path forwarded upstream (after suffix handling).
    pub path: Option<String>,
    /// Raw query string forwarded upstream.
    pub query: Option<String>,
    /// Outbound method.
    pub method: Option<String>,
    /// Outbound headers (mutable).
    pub headers: BTreeMap<String, Vec<String>>,
    /// Outbound body (mutable; plugins may rewrite it).
    pub body: Vec<u8>,
    /// Caller tenant.
    pub tenant_id: Option<String>,
    /// Authenticated principal id, when the host produced one.
    pub principal_id: Option<String>,
    /// Client IP of the caller, when known.
    pub client_ip: Option<String>,
    /// Request correlation id.
    pub request_id: Option<String>,
    /// Per-request scratch space plugins may use to pass state along.
    pub attributes: BTreeMap<String, String>,
    /// Plugin configuration of the binding currently being executed.
    pub plugin_config: Option<serde_json::Value>,
}

impl RequestContext {
    /// Reads the first value of a header, case-insensitively.
    pub fn header(&self, name: &str) -> Option<&str> {
        let lower = name.to_ascii_lowercase();
        self.headers.iter().find(|(k, _)| k.to_ascii_lowercase() == lower).and_then(|(_, v)| v.first().map(|s| s.as_str()))
    }

    /// Sets a header to a single value, replacing any existing entries.
    pub fn set_header(&mut self, name: impl Into<String>, value: impl Into<String>) {
        self.headers.insert(name.into(), vec![value.into()]);
    }

    /// Appends a value to a header.
    pub fn add_header(&mut self, name: impl Into<String>, value: impl Into<String>) {
        self.headers.entry(name.into()).or_default().push(value.into());
    }

    /// Removes a header, case-insensitively.
    pub fn remove_header(&mut self, name: &str) {
        let lower = name.to_ascii_lowercase();
        self.headers.retain(|k, _| k.to_ascii_lowercase() != lower);
    }
}

/// Mutable view of the inbound response handed to plugins.
#[derive(Debug, Clone, Default)]
pub struct ResponseContext {
    /// Upstream response status.
    pub status: Option<u16>,
    /// Response headers (mutable).
    pub headers: BTreeMap<String, Vec<String>>,
    /// Response body (mutable).
    pub body: Vec<u8>,
    /// Per-request scratch space shared with the request phase.
    pub attributes: BTreeMap<String, String>,
    /// Plugin configuration of the binding currently being executed.
    pub plugin_config: Option<serde_json::Value>,
}

impl ResponseContext {
    /// Reads the first value of a header, case-insensitively.
    pub fn header(&self, name: &str) -> Option<&str> {
        let lower = name.to_ascii_lowercase();
        self.headers.iter().find(|(k, _)| k.to_ascii_lowercase() == lower).and_then(|(_, v)| v.first().map(|s| s.as_str()))
    }

    /// Sets a header to a single value, replacing any existing entries.
    pub fn set_header(&mut self, name: impl Into<String>, value: impl Into<String>) {
        self.headers.insert(name.into(), vec![value.into()]);
    }
}

/// Mutable view of a gateway error handed to transform plugins.
#[derive(Debug, Clone, Default)]
pub struct ErrorContext {
    /// The error about to be rendered.
    pub error: Option<DomainError>,
    /// Whether the gateway or the upstream produced the failure.
    pub source: Option<String>,
    /// Per-request scratch space shared with the request phase.
    pub attributes: BTreeMap<String, String>,
    /// Headers a transform wants stamped onto the rendered problem.
    pub headers: BTreeMap<String, Vec<String>>,
}

impl ErrorContext {
    /// Sets a header to a single value, replacing any existing entries.
    pub fn set_header(&mut self, name: impl Into<String>, value: impl Into<String>) {
        self.headers.insert(name.into(), vec![value.into()]);
    }
}

/// Injects credentials into the outbound request. One per upstream.
#[async_trait]
pub trait AuthPlugin: Send + Sync {
    /// Plugin identifier (GTS instance or registry key).
    fn id(&self) -> &str;

    /// The GTS plugin type the plugin implements.
    fn plugin_type(&self) -> &str;

    /// Injects credentials into `ctx`.
    async fn authenticate(&self, ctx: &mut RequestContext) -> Result<(), PluginError>;
}

/// Validates requests and enforces policy; may reject.
#[async_trait]
pub trait GuardPlugin: Send + Sync {
    /// Plugin identifier (GTS instance or registry key).
    fn id(&self) -> &str;

    /// The GTS plugin type the plugin implements.
    fn plugin_type(&self) -> &str;

    /// Validates the outbound request.
    async fn guard_request(&self, ctx: &RequestContext) -> Result<GuardDecision, PluginError>;

    /// Validates the upstream response.
    async fn guard_response(&self, ctx: &ResponseContext) -> Result<GuardDecision, PluginError>;
}

/// Modifies request, response and error data.
#[async_trait]
pub trait TransformPlugin: Send + Sync {
    /// Plugin identifier (GTS instance or registry key).
    fn id(&self) -> &str;

    /// The GTS plugin type the plugin implements.
    fn plugin_type(&self) -> &str;

    /// Mutates the outbound request.
    async fn transform_request(&self, ctx: &mut RequestContext) -> Result<(), PluginError>;

    /// Mutates the inbound response.
    async fn transform_response(&self, ctx: &mut ResponseContext) -> Result<(), PluginError>;

    /// Mutates a gateway error before it is rendered.
    async fn transform_error(&self, ctx: &mut ErrorContext) -> Result<(), PluginError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guard_decision_is_object_safe_and_inspectable() {
        assert!(GuardDecision::Next.is_next());
        assert!(!GuardDecision::reject(DomainError::not_found("x")).is_next());
    }

    #[test]
    fn plugin_error_maps_to_a_domain_error() {
        let err: DomainError =
            PluginError::failure("p", "boom").into();
        assert!(matches!(err, DomainError::Internal { .. }));

        let rejected: DomainError =
            PluginError::Reject(DomainError::validation("bad")).into();
        assert!(matches!(rejected, DomainError::ValidationError { .. }));
    }

    #[test]
    fn request_context_header_helpers_are_case_insensitive() {
        let mut ctx = RequestContext::default();
        ctx.set_header("X-Api-Key", "value");
        assert_eq!(ctx.header("x-api-key"), Some("value"));
        ctx.add_header("X-Api-Key", "second");
        assert_eq!(ctx.headers.get("X-Api-Key").map(Vec::len), Some(2));
        ctx.remove_header("X-API-KEY");
        assert!(ctx.headers.is_empty());
    }
}
