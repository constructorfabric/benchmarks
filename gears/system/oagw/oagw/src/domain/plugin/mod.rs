//! Plugin contracts (`ADR/0002-plugin-system.md`).
//!
//! Three plugin types with separate traits — Auth, Guard and Transform — with
//! the same traits for built-in and external plugins. The data plane executes
//! them in the documented order:
//!
//! ```text
//! auth → guards → request transforms → upstream call → response transforms
//! ```

use std::sync::Arc;

use async_trait::async_trait;
use http::HeaderMap;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::domain::error::DomainError;

/// Result type every plugin method returns.
pub type PluginResult<T> = Result<T, DomainError>;

/// Everything a plugin may need beyond the request payload.
#[derive(Clone)]
pub struct PluginRuntime {
    /// Caller identity, supplied by the platform's authentication middleware.
    pub security: SecurityContext,
    /// Caller tenant.
    pub tenant_id: Uuid,
    /// Authenticated subject.
    pub subject_id: Uuid,
    /// Credential store used to resolve `secret_ref` values.
    pub credstore: Option<Arc<dyn credstore_sdk::CredStoreClientV1>>,
    /// Outbound client used for the OAuth2 token endpoint call.
    pub http: Arc<toolkit_http::HttpClient>,
    /// Timeout applied to plugin-issued outbound calls.
    pub timeout: std::time::Duration,
}

/// Request-side plugin context.
pub struct RequestContext {
    /// Upstream-bound method.
    pub method: http::Method,
    /// Upstream-bound path, after route resolution.
    pub path: String,
    /// Raw query string, after allow-list filtering.
    pub query: String,
    /// Headers to forward, already stripped of hop-by-hop entries.
    pub headers: HeaderMap,
    /// Request body.
    pub body: bytes::Bytes,
    /// Correlation identifier.
    pub request_id: Option<String>,
    /// Configuration of the plugin binding being executed.
    pub config: serde_json::Value,
    /// Shared plugin services.
    pub runtime: Arc<PluginRuntime>,
}

/// Response-side plugin context.
pub struct ResponseContext {
    /// Upstream status.
    pub status: u16,
    /// Response headers.
    pub headers: HeaderMap,
    /// Correlation identifier.
    pub request_id: Option<String>,
    /// Configuration of the plugin binding being executed.
    pub config: serde_json::Value,
    /// Shared plugin services.
    pub runtime: Arc<PluginRuntime>,
}

/// Error-side plugin context.
pub struct ErrorContext {
    /// The failure about to be rendered.
    pub error: Option<DomainError>,
    /// Headers to add to the rendered problem response.
    pub headers: HeaderMap,
    /// Correlation identifier.
    pub request_id: Option<String>,
    /// Configuration of the plugin binding being executed.
    pub config: serde_json::Value,
    /// Shared plugin services.
    pub runtime: Arc<PluginRuntime>,
}

/// Guard verdict.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GuardDecision {
    /// Proceed.
    Allow,
    /// Reject with a gateway error.
    Reject(DomainError),
}

impl GuardDecision {
    /// `true` when the guard allows the request.
    #[must_use]
    pub fn is_allow(&self) -> bool {
        matches!(self, Self::Allow)
    }
}

/// Credential injection (`gts.cf.core.oagw.auth_plugin.v1~*`).
///
/// Executed once per request, before guards.
#[async_trait]
pub trait AuthPlugin: Send + Sync {
    /// Instance identifier the plugin is registered under.
    fn id(&self) -> &str;

    /// Plugin family, one of `auth`, `guard`, `transform`.
    fn plugin_type(&self) -> &str {
        "auth"
    }

    /// Injects credentials into the outgoing request.
    ///
    /// # Errors
    /// [`DomainError::AuthenticationFailed`] when credentials cannot be
    /// established, [`DomainError::SecretNotFound`] when a referenced secret
    /// is missing.
    async fn authenticate(&self, ctx: &mut RequestContext) -> PluginResult<()>;
}

/// Validation / policy enforcement (`gts.cf.core.oagw.guard_plugin.v1~*`).
///
/// Executed after auth, before request transforms.
#[async_trait]
pub trait GuardPlugin: Send + Sync {
    /// Instance identifier the plugin is registered under.
    fn id(&self) -> &str;

    /// Plugin family.
    fn plugin_type(&self) -> &str {
        "guard"
    }

    /// Validates the outgoing request.
    ///
    /// # Errors
    /// Any [`DomainError`] the guard needs to surface.
    async fn guard_request(&self, ctx: &RequestContext) -> PluginResult<GuardDecision>;

    /// Validates the upstream response.
    ///
    /// # Errors
    /// Any [`DomainError`] the guard needs to surface.
    async fn guard_response(&self, ctx: &ResponseContext) -> PluginResult<GuardDecision>;
}

/// Request/response mutation (`gts.cf.core.oagw.transform_plugin.v1~*`).
#[async_trait]
pub trait TransformPlugin: Send + Sync {
    /// Instance identifier the plugin is registered under.
    fn id(&self) -> &str;

    /// Plugin family.
    fn plugin_type(&self) -> &str {
        "transform"
    }

    /// Mutates the outgoing request.
    ///
    /// # Errors
    /// Any [`DomainError`] the transform needs to surface.
    async fn transform_request(&self, ctx: &mut RequestContext) -> PluginResult<()>;

    /// Mutates the response before it is returned to the client.
    ///
    /// # Errors
    /// Any [`DomainError`] the transform needs to surface.
    async fn transform_response(&self, ctx: &mut ResponseContext) -> PluginResult<()>;

    /// Mutates a gateway error before it is rendered.
    ///
    /// # Errors
    /// Any [`DomainError`] the transform needs to surface.
    async fn transform_error(&self, ctx: &mut ErrorContext) -> PluginResult<()>;
}

#[cfg(any(test, feature = "test-utils"))]
pub(crate) mod test_support {
    //! Fixtures shared by the plugin unit tests and the integration tests.

    use std::sync::Arc;
    use std::time::Duration;
    use uuid::Uuid;

    use super::RequestContext;
    use crate::domain::plugin::PluginRuntime;

    /// An outbound HTTP client with no pooling requirements, for tests.
    fn test_http() -> Arc<toolkit_http::HttpClient> {
        Arc::new(
            toolkit_http::HttpClientBuilder::default()
                .build()
                .expect("test http client"),
        )
    }

    /// A [`PluginRuntime`] with an anonymous security context and no
    /// credential store.
    #[must_use]
    pub fn test_runtime() -> Arc<PluginRuntime> {
        Arc::new(PluginRuntime {
            security: toolkit_security::SecurityContext::anonymous(),
            tenant_id: Uuid::nil(),
            subject_id: Uuid::nil(),
            credstore: None,
            http: test_http(),
            timeout: Duration::from_secs(5),
        })
    }

    /// A [`ResponseContext`] with an empty header map.
    #[must_use]
    pub fn test_response(config: serde_json::Value) -> super::ResponseContext {
        super::ResponseContext {
            status: 200,
            headers: http::HeaderMap::new(),
            request_id: None,
            config,
            runtime: test_runtime(),
        }
    }

    /// A [`RequestContext`] with an empty header map and body.
    #[must_use]
    pub fn test_request(config: serde_json::Value) -> RequestContext {
        RequestContext {
            method: http::Method::GET,
            path: "/".to_owned(),
            query: String::new(),
            headers: http::HeaderMap::new(),
            body: bytes::Bytes::new(),
            request_id: None,
            config,
            runtime: test_runtime(),
        }
    }
}

#[cfg(any(test, feature = "test-utils"))]
pub use test_support::{test_request, test_response, test_runtime};
