//! Plugin system — three plugin types with trait-based extensibility
//! (ADR-0002).
//!
//! The traits below are the verbatim ADR-0002 shapes: `AuthPlugin`
//! (credential injection), `GuardPlugin` (validation, can reject) and
//! `TransformPlugin` (request/response/error mutation). Execution order in
//! the data plane is:
//!
//! `Auth → Guard(request) → Transform(request) → upstream call →
//! Transform(response / error) → Guard(response)`.

pub use async_trait::async_trait;
use serde_json::Map;

use crate::domain::dto::Endpoint;

use super::error::DomainError;

/// A plugin's bound configuration (the `config` object from its binding).
pub type PluginConfig = Map<String, serde_json::Value>;

/// Shared error type for plugin execution.
#[derive(Debug, thiserror::Error, Clone)]
pub enum PluginError {
    /// Invalid plugin configuration (checked before/while running).
    #[error("plugin configuration error: {detail}")]
    Config { detail: String, code: &'static str },
    /// The plugin rejected the request/response.
    #[error("plugin rejected: {detail}")]
    Reject {
        detail: String,
        code: &'static str,
        status: u16,
    },
    /// Internal plugin failure (should be mapped to a 5xx upstream-side error).
    #[error("plugin internal error: {detail}")]
    Internal { detail: String },
    /// Authentication to the upstream failed.
    #[error("plugin authentication failed: {detail}")]
    AuthFailed { detail: String },
    /// A referenced credential could not be resolved from the credential
    /// store (maps to the 500 `secret.not_found.v1` problem).
    #[error("referenced secret not found: {detail}")]
    SecretNotFound { detail: String },
}

impl PluginError {
    #[must_use]
    pub fn config(detail: impl Into<String>) -> Self {
        Self::Config {
            detail: detail.into(),
            code: "PLUGIN_CONFIG_INVALID",
        }
    }

    /// Map a plugin error onto the appropriate domain error for the caller.
    #[must_use]
    pub fn into_domain_error(self) -> DomainError {
        match self {
            Self::Config { detail, .. } => DomainError::validation(detail),
            Self::Internal { detail } => DomainError::LinkUnavailable {
                detail,
                context: Option::default(),
            },
            Self::AuthFailed { detail } => DomainError::AuthFailed {
                detail,
                context: Option::default(),
            },
            Self::SecretNotFound { detail } => DomainError::SecretNotFound {
                detail,
                context: Option::default(),
            },
            Self::Reject {
                detail,
                code,
                status,
            } => match status {
                401 => DomainError::AuthFailed {
                    detail: if detail.is_empty() {
                        format!("request rejected by plugin ({code})")
                    } else {
                        detail
                    },
                    context: Option::default(),
                },
                429 => DomainError::RateLimitExceeded {
                    detail,
                    retry_after_seconds: 1,
                    context: Option::default(),
                },
                502 => DomainError::DownstreamError {
                    detail,
                    context: Option::default(),
                },
                _ => DomainError::Validation {
                    detail: if detail.is_empty() {
                        format!("request rejected by plugin ({code})")
                    } else {
                        detail
                    },
                    context: Option::default(),
                },
            },
        }
    }
}

/// The decision a guard plugin returns from a phase.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GuardDecision {
    /// Continue with the request/response.
    Allow,
    /// Reject with a status code, error code and detail.
    Reject {
        status: u16,
        code: &'static str,
        detail: String,
    },
}

/// Mutable request context passed through the auth / transform phases.
///
/// Plugins may mutate the method, path, query, headers and body; the data
/// plane serializes the resulting values into the outbound request.
#[derive(Debug, Clone)]
pub struct RequestContext {
    /// HTTP method of the outbound request.
    pub method: http::Method,
    /// Outbound path (route path + appended suffix).
    pub path: String,
    /// Outbound query string (without leading `?`).
    pub query: String,
    /// Outbound request headers.
    pub headers: http::HeaderMap,
    /// Buffered request body (buffered by the proxy handler).
    pub body: bytes::Bytes,
    /// The caller's security context (used by plugins to resolve tenant /
    /// subject-scoped secrets from the credential store).
    pub security_context: toolkit_security::SecurityContext,
    /// Id of the upstream being targeted.
    pub upstream_id: uuid::Uuid,
    /// Alias the request resolved to.
    pub upstream_alias: String,
    /// Tenant that owns the resolved upstream.
    pub tenant_id: uuid::Uuid,
    /// The bound plugin's configuration.
    pub config: PluginConfig,
    /// The endpoint selected for this request (when selection is complete).
    pub endpoint: Option<Endpoint>,
}

impl RequestContext {
    /// Convenience constructor for tests and non-http affordances.
    #[must_use]
    pub fn new() -> Self {
        Self {
            method: http::Method::GET,
            path: "/".to_owned(),
            query: String::new(),
            headers: http::HeaderMap::new(),
            body: bytes::Bytes::new(),
            security_context: toolkit_security::SecurityContext::anonymous(),
            upstream_id: uuid::Uuid::nil(),
            upstream_alias: String::new(),
            tenant_id: uuid::Uuid::nil(),
            config: PluginConfig::new(),
            endpoint: None,
        }
    }
}

impl Default for RequestContext {
    fn default() -> Self {
        Self::new()
    }
}

/// Read-only or mutable response context for guard/transform response
/// phases.
#[derive(Debug, Clone)]
pub struct ResponseContext {
    /// Upstream HTTP status (immutable — guards/transforms may only inspect).
    pub status: http::StatusCode,
    /// Response headers (mutable by transform).
    pub headers: http::HeaderMap,
    /// Id of the upstream that produced the response.
    pub upstream_id: uuid::Uuid,
    /// Alias the request resolved to.
    pub upstream_alias: String,
    /// The bound plugin's configuration.
    pub config: PluginConfig,
}

impl ResponseContext {
    /// Convenience constructor for tests.
    #[must_use]
    pub fn new(status: http::StatusCode) -> Self {
        Self {
            status,
            headers: http::HeaderMap::new(),
            upstream_id: uuid::Uuid::nil(),
            upstream_alias: String::new(),
            config: PluginConfig::new(),
        }
    }
}

/// Error context passed to `TransformPlugin::transform_error`.
#[derive(Debug, Clone)]
pub struct ErrorContext {
    /// Upstream / alias context when known.
    pub upstream_id: Option<uuid::Uuid>,
    pub alias: Option<String>,
    /// Error status code (may be mutated).
    pub status: u16,
    /// Error code (may be mutated).
    pub code: String,
    /// Error detail (may be mutated).
    pub detail: String,
    /// The bound plugin's configuration.
    pub config: PluginConfig,
}

/// Type alias for auth plugin results.
pub type PluginResult<T> = Result<T, PluginError>;

/// Auth plugin — injects credentials into outbound requests (ADR-0002).
#[async_trait]
pub trait AuthPlugin: Send + Sync {
    fn id(&self) -> &str;
    fn plugin_type(&self) -> &str;
    async fn authenticate(&self, ctx: &mut RequestContext) -> PluginResult<()>;
}

/// Guard plugin — validates requests and enforces policies (ADR-0002).
#[async_trait]
pub trait GuardPlugin: Send + Sync {
    fn id(&self) -> &str;
    fn plugin_type(&self) -> &str;
    async fn guard_request(&self, ctx: &RequestContext) -> PluginResult<GuardDecision>;
    async fn guard_response(&self, ctx: &ResponseContext) -> PluginResult<GuardDecision>;
}

/// Transform plugin — modifies request/response/error data (ADR-0002).
#[async_trait]
pub trait TransformPlugin: Send + Sync {
    fn id(&self) -> &str;
    fn plugin_type(&self) -> &str;
    async fn transform_request(&self, ctx: &mut RequestContext) -> PluginResult<()>;
    async fn transform_response(&self, ctx: &mut ResponseContext) -> PluginResult<()>;
    async fn transform_error(&self, ctx: &mut ErrorContext) -> PluginResult<()>;
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[allow(dead_code)] // trait conformance probe; never instantiated in tests
    struct TestAuthPlugin;

    #[async_trait]
    impl AuthPlugin for TestAuthPlugin {
        fn id(&self) -> &'static str {
            "test"
        }
        fn plugin_type(&self) -> &'static str {
            "gts...~test.v1"
        }
        async fn authenticate(&self, _ctx: &mut RequestContext) -> PluginResult<()> {
            Ok(())
        }
    }

    #[test]
    fn plugin_error_mappings() {
        let e = PluginError::Reject {
            detail: "missing header".into(),
            code: "REQUIRED_HEADER_MISSING",
            status: 400,
        };
        let de = e.into_domain_error();
        assert_eq!(de.problem_info().status, 400);

        let e = PluginError::Reject {
            detail: "no".into(),
            code: "X",
            status: 401,
        };
        let de = e.into_domain_error();
        assert_eq!(de.problem_info().status, 401);
        assert_eq!(
            de.problem_info().type_id,
            super::super::gts_helpers::ERROR_AUTH_FAILED
        );
    }
}
