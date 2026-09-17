//! Plugin contracts (ADR-0002 Plugin System).
//!
//! Execution order for a proxied request:
//!   auth → guard(request) → transform(request) → upstream call
//!         → transform(response) / transform(error) → guard(response)
//!
//! Upstream plugins execute before route plugins: with upstream bindings
//! `[U1, U2]` and route bindings `[R1, R2]`, the merged chain is
//! `[U1, U2, R1, R2]`.

use std::sync::Arc;

use async_trait::async_trait;
use axum::http::{HeaderMap, Method};
use bytes::Bytes;
use serde_json::Value;

/// Outcome of a guard decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GuardDecision {
    /// Allow the request/response to proceed.
    Allow,
    /// Reject with a gateway error.
    Reject(GuardRejectInfo),
}

/// Details of a guard rejection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuardRejectInfo {
    /// HTTP status code for the rejection.
    pub status: u16,
    /// RFC 9457 `type` GTS identifier.
    pub problem_type: &'static str,
    /// Human-readable detail.
    pub detail: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GuardRejectReason {
    /// A required header was missing (`problem_type: ERR_REQUIRED_HEADER_MISSING`).
    RequiredHeaderMissing { name: String },
    /// Generic rejection.
    Other { status: u16, detail: String },
}

/// Shared per-request context passed into plugin execution.
#[derive(Debug, Clone, Default)]
pub struct RequestContext {
    /// Calling tenant id (hex) — used for tenant-scoped plugin state and
    /// OAuth2 token cache keys.
    pub tenant_id: uuid::Uuid,
    /// Calling subject id.
    pub subject_id: uuid::Uuid,
    /// Token scopes of the calling principal (used for credstore access).
    pub scopes: Vec<String>,
    /// Caller IP (when known).
    pub client_ip: Option<std::net::IpAddr>,
    /// Outbound request being built for the upstream.
    pub method: Method,
    pub uri: String,
    pub headers: HeaderMap,
    /// Buffered request body.
    pub body: Bytes,
    /// Effective target host (from `X-OAGW-Target-Host` or derived).
    pub target_host: Option<String>,
    /// Opaque per-plugin scratch space (plugin-private key = plugin id).
    pub metadata: std::collections::HashMap<String, Value>,
    /// Plugin configuration for the running plugin.
    pub plugin_config: Option<Value>,
}

/// Response context for guard/transform response phases.
#[derive(Debug, Clone, Default)]
pub struct ResponseContext {
    pub status: u16,
    pub headers: HeaderMap,
    /// Streamed-upstream response body is not buffered; plugins only see
    /// headers for the response phase. Set when available.
    pub body_bytes: Option<Bytes>,
    pub metadata: std::collections::HashMap<String, Value>,
    pub plugin_config: Option<Value>,
}

/// Error context for transform_error phase.
#[derive(Debug, Clone, Default)]
pub struct ErrorContext {
    pub status: u16,
    pub problem_type: Option<String>,
    pub detail: Option<String>,
    pub headers: HeaderMap,
    pub metadata: std::collections::HashMap<String, Value>,
    pub plugin_config: Option<Value>,
}

/// Plugin execution error.
#[derive(Debug, thiserror::Error)]
pub enum PluginError {
    /// The plugin could not resolve required secrets/credentials.
    #[error("plugin authentication failed: {0}")]
    AuthFailed(String),
    /// The plugin configuration is invalid for the request.
    #[error("invalid plugin configuration: {0}")]
    Config(String),
    /// Upstream auth exchange failed (e.g. OAuth2 token endpoint error).
    #[error("plugin upstream error: {0}")]
    Upstream(String),
}

/// Authentication plugin (ADR-0002).
#[async_trait]
pub trait AuthPlugin: Send + Sync {
    /// Canonical GTS identifier of this plugin.
    fn id(&self) -> &'static str;

    /// GTS type of this plugin category.
    fn plugin_type(&self) -> &'static str {
        crate::gts_helpers::AUTH_PLUGIN_TYPE
    }

    /// Mutate the outbound request to carry credentials. Failure produces an
    /// auth error on the data plane. Async because credential material is
    /// resolved via the credential store (and OAuth2 tokens are fetched over
    /// the network).
    async fn authenticate(&self, ctx: &mut RequestContext) -> Result<(), PluginError>;
}

/// Guard plugin (ADR-0002).
pub trait GuardPlugin: Send + Sync {
    fn id(&self) -> &'static str;

    fn plugin_type(&self) -> &'static str {
        crate::gts_helpers::GUARD_PLUGIN_TYPE
    }

    /// Inspect the outbound request; return a decision.
    fn guard_request(&self, ctx: &RequestContext) -> Result<GuardDecision, PluginError>;

    /// Inspect the upstream response; return a decision.
    fn guard_response(&self, ctx: &ResponseContext) -> Result<GuardDecision, PluginError>;
}

/// Transform plugin (ADR-0002).
pub trait TransformPlugin: Send + Sync {
    fn id(&self) -> &'static str;

    fn plugin_type(&self) -> &'static str {
        crate::gts_helpers::TRANSFORM_PLUGIN_TYPE
    }

    /// Mutate the outbound request.
    fn transform_request(&self, ctx: &mut RequestContext) -> Result<(), PluginError>;

    /// Mutate the upstream response (headers only; body is streamed).
    fn transform_response(&self, ctx: &mut ResponseContext) -> Result<(), PluginError>;

    /// Adjust an error before it is returned to the caller.
    fn transform_error(&self, _ctx: &mut ErrorContext) -> Result<(), PluginError> {
        Ok(())
    }
}

/// Convenience alias so infra can hold the concrete registries.
pub type ArcAuthPlugin = Arc<dyn AuthPlugin>;
pub type ArcGuardPlugin = Arc<dyn GuardPlugin>;
pub type ArcTransformPlugin = Arc<dyn TransformPlugin>;
