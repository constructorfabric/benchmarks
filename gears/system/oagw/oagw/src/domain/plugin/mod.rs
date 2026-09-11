// Updated: 2026-09-01 by Constructor Tech
//! Plugin contracts (ADR-0002).
//!
//! A plugin is a named unit of behaviour bound to an upstream or a route and
//! executed in three phases: **auth** (once, before everything), **guard**
//! (request and response), and **transform** (request, response, error). The
//! traits here are the seam between the Data Plane and the plugin
//! implementations in `infra/plugin/`.
//!
//! Ordering is fixed by the engine: auth plugins run first, then guards, then
//! request transforms. Upstream-bound plugins run before route-bound plugins
//! at every phase.

use std::collections::BTreeMap;

use async_trait::async_trait;
use bytes::Bytes;
use toolkit_security::SecurityContext;

/// Everything a plugin needs to know about the request in flight.
///
/// `header_map` is the *outbound* header set: the engine applies the upstream
/// header rules before plugins run, so a transform sees and mutates exactly
/// what will be sent.
#[derive(Debug, Clone)]
pub struct RequestContext {
    /// Stable identifier for this proxy exchange (also the `trace_id` in
    /// problem documents).
    pub request_id: String,
    /// Caller security context resolved by the inbound gateway.
    pub security_context: SecurityContext,
    /// Tenant the effective configuration was resolved for.
    pub tenant_id: uuid::Uuid,
    /// The alias that matched.
    pub alias: String,
    /// GTS identifier of the upstream being proxied to.
    pub upstream_id: String,
    /// Path that will be sent upstream, after suffix handling.
    pub path: String,
    /// Raw query string, without the leading `?`.
    pub query: String,
    /// Inbound method.
    pub method: http::Method,
    /// Outbound headers (already filtered and rewritten).
    pub headers: http::HeaderMap,
    /// Upstream/route plugin configuration, merged.
    pub config: BTreeMap<String, serde_json::Value>,
    /// Fully-buffered body, when the request is not streaming.
    pub body: Bytes,
}

impl RequestContext {
    /// Caller tenant id, or `None` when the caller is anonymous.
    ///
    /// `SecurityContext` models "no tenant" as the nil UUID, so the nil check
    /// is the whole of the logic here.
    #[must_use]
    pub fn subject_tenant_id(&self) -> Option<uuid::Uuid> {
        let id = self.security_context.subject_tenant_id();
        (!id.is_nil()).then_some(id)
    }

    /// Caller subject id, or `None` when the caller is anonymous.
    #[must_use]
    pub fn subject_id(&self) -> Option<uuid::Uuid> {
        let id = self.security_context.subject_id();
        (!id.is_nil()).then_some(id)
    }
}

/// What a guard decided.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GuardDecision {
    /// Continue.
    Allow,
    /// Stop with this status code and reason.
    Reject {
        status: http::StatusCode,
        code: String,
    },
}

impl GuardDecision {
    #[must_use]
    pub fn reject(status: http::StatusCode, code: impl Into<String>) -> Self {
        Self::Reject {
            status,
            code: code.into(),
        }
    }
}

/// Everything a plugin may change about the upstream response.
#[derive(Debug, Clone)]
pub struct ResponseContext {
    pub status: http::StatusCode,
    pub headers: http::HeaderMap,
    /// Always `None` for streamed bodies: a plugin may only rewrite a fully
    /// buffered response.
    pub body: Option<Bytes>,
    /// The binding's plugin configuration, plus the exchange's `request_id`.
    pub config: BTreeMap<String, serde_json::Value>,
}

/// What went wrong, handed to `transform_error`.
#[derive(Debug, Clone)]
pub struct ErrorContext {
    pub status: http::StatusCode,
    pub error_type: &'static str,
    pub detail: String,
}

/// The failure a plugin reports.
#[derive(Debug, Clone, thiserror::Error)]
pub enum PluginError {
    /// The plugin wants the request rejected with this status and reason.
    #[error("{code}: {message}")]
    Rejected {
        status: http::StatusCode,
        code: String,
        message: String,
    },
    /// Infrastructure failure (missing credential, unreachable token endpoint).
    #[error("{0}")]
    Infrastructure(String),
}

/// Authentication plugin (ADR-0002 `AuthPlugin`).
#[async_trait]
pub trait AuthPlugin: Send + Sync + std::fmt::Debug {
    /// Stable identifier, used for logging only.
    fn id(&self) -> &str;
    /// The plugin type this implementation serves.
    fn plugin_type(&self) -> &'static str;
    /// Inject credentials into the outbound request, or report a rejection.
    ///
    /// # Errors
    ///
    /// [`PluginError::Rejected`] when the caller is not authorized;
    /// [`PluginError::Infrastructure`] when credentials cannot be resolved.
    async fn authenticate(&self, ctx: &mut RequestContext) -> Result<(), PluginError>;
}

/// Guard plugin (ADR-0002 `GuardPlugin`).
#[async_trait]
pub trait GuardPlugin: Send + Sync + std::fmt::Debug {
    fn id(&self) -> &str;
    fn plugin_type(&self) -> &'static str;
    /// Validate the request before it leaves the gateway.
    ///
    /// # Errors
    ///
    /// [`PluginError::Rejected`] when the request must not be proxied.
    async fn guard_request(&self, ctx: &RequestContext) -> Result<GuardDecision, PluginError>;
    /// Validate the response before it reaches the caller.
    ///
    /// # Errors
    ///
    /// [`PluginError::Rejected`] when the response must not be returned.
    async fn guard_response(&self, ctx: &ResponseContext) -> Result<GuardDecision, PluginError>;
}

/// Transform plugin (ADR-0002 `TransformPlugin`).
#[async_trait]
pub trait TransformPlugin: Send + Sync + std::fmt::Debug {
    fn id(&self) -> &str;
    fn plugin_type(&self) -> &'static str;
    /// Mutate the outbound request.
    ///
    /// # Errors
    ///
    /// [`PluginError`] when the transformation fails.
    async fn transform_request(&self, ctx: &mut RequestContext) -> Result<(), PluginError>;
    /// Mutate the inbound response.
    ///
    /// # Errors
    ///
    /// [`PluginError`] when the transformation fails.
    async fn transform_response(&self, ctx: &mut ResponseContext) -> Result<(), PluginError>;
    /// Observe (or annotate) a gateway-generated error.
    ///
    /// # Errors
    ///
    /// [`PluginError`] when the transformation fails.
    async fn transform_error(&self, ctx: &mut ErrorContext) -> Result<(), PluginError>;
}
