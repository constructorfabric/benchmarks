//! Contexts handed to plugins, per `docs/ADR/0002-plugin-system.md`.
//!
//! Plugins mutate a [`RequestContext`] before the outbound call and a
//! [`ResponseContext`] or [`ErrorContext`] after it. No secret material is ever
//! placed in a context: values resolved from the credential store are injected
//! straight into the outbound header map and never logged.

use std::collections::HashMap;

use toolkit_security::SecurityContext;

use crate::domain::error::DomainError;

/// Request context flowing through the plugin chain.
#[derive(Debug, Clone)]
pub struct RequestContext {
    /// HTTP method of the incoming request.
    pub method: String,
    /// Path forwarded to the upstream.
    pub path: String,
    /// Raw query string, forwarded verbatim.
    pub query: String,
    /// Request headers, mutable so plugins may inject credentials.
    pub headers: http::HeaderMap,
    /// Whether the request carries a body.
    pub body_present: bool,
    /// Authenticated caller.
    pub security_context: SecurityContext,
    /// Tenant scope used for alias and limit resolution.
    pub tenant_scope: Vec<uuid::Uuid>,
    /// Header names this chain has injected, for observability.
    pub injected_headers: Vec<String>,
    /// Free-form attributes plugins may use to hand state downstream.
    pub attributes: HashMap<String, String>,
}

impl RequestContext {
    /// Set a request header, recording the injection.
    pub fn set_header(&mut self, name: &str, value: &str) {
        if let (Ok(name), Ok(value)) = (
            http::HeaderName::from_bytes(name.as_bytes()),
            http::HeaderValue::from_str(value),
        ) {
            self.headers.insert(&name, value);
            self.injected_headers.push(name.to_string());
        }
    }

    /// Record an attribute for downstream plugins.
    pub fn set_attribute(&mut self, key: impl Into<String>, value: impl Into<String>) {
        self.attributes.insert(key.into(), value.into());
    }
}

/// Response context flowing back through the plugin chain.
#[derive(Debug, Clone)]
pub struct ResponseContext {
    /// Upstream status code.
    pub status: http::StatusCode,
    /// Response headers, mutable so plugins may add to them.
    pub headers: http::HeaderMap,
    /// Header names this chain has injected.
    pub injected_headers: Vec<String>,
}

impl ResponseContext {
    /// Set a response header, recording the injection.
    pub fn set_header(&mut self, name: &str, value: &str) {
        if let (Ok(name), Ok(value)) = (
            http::HeaderName::from_bytes(name.as_bytes()),
            http::HeaderValue::from_str(value),
        ) {
            self.headers.insert(&name, value);
            self.injected_headers.push(name.to_string());
        }
    }
}

/// Error context handed to `transform_error`.
#[derive(Debug, Clone)]
pub struct ErrorContext {
    /// The gateway error about to be rendered.
    pub error: DomainError,
    /// Headers to attach to the error response.
    pub headers: http::HeaderMap,
}

impl ErrorContext {
    /// Set a header on the error response.
    pub fn set_header(&mut self, name: &str, value: &str) {
        if let (Ok(name), Ok(value)) = (
            http::HeaderName::from_bytes(name.as_bytes()),
            http::HeaderValue::from_str(value),
        ) {
            self.headers.insert(name, value);
        }
    }
}

/// Outcome of a guard plugin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GuardDecision {
    /// Let the exchange continue.
    Allow,
    /// Short-circuit with this error.
    Reject(DomainError),
}

impl GuardDecision {
    /// Whether the exchange may continue.
    #[must_use]
    pub const fn is_allowed(&self) -> bool {
        matches!(self, Self::Allow)
    }

    /// The rejection error, when the guard rejected.
    #[must_use]
    pub const fn rejection(&self) -> Option<&DomainError> {
        match self {
            Self::Allow => None,
            Self::Reject(err) => Some(err),
        }
    }
}
