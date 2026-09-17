//! Plugin system: three plugin types with deterministic execution order
//! (ADR-0002): Auth → Guards → Transform(on_request) → upstream →
//! Transform(on_response/on_error).
//!
//! Plugins receive phase-scoped contexts and reject via
//! [`PluginError`]. The domain layer defines the traits; the built-in
//! implementations live in [`crate::infra::plugins`].

use async_trait::async_trait;
use axum::http::{HeaderMap, Method};
use credstore_sdk::CredStoreClientV1;
use std::sync::Arc;
use toolkit_security::SecurityContext;

use crate::error::OagwError;

/// A rejection produced while a plugin runs.
#[derive(Debug, Clone)]
pub enum PluginError {
    /// The plugin decided the request/response must be rejected. `error_code`
    /// is an OAGW-specific code (e.g. `REQUIRED_HEADER_MISSING`); the error
    /// is surfaced as a gateway problem response.
    Reject {
        status: u16,
        error_code: String,
        detail: String,
    },
    /// Unable to resolve a required dependency (secret, upstream, …).
    Internal(String),
    /// The plugin itself could not be resolved from its identifier.
    Unknown(String),
}

impl PluginError {
    /// Convert into the OAGW error contract. `Reject` maps onto specific
    /// documented error types by status; everything else is auth/plugin
    /// failure semantics.
    pub fn into_oagw_error(self, plugin_ref: &str) -> OagwError {
        match self {
            PluginError::Reject {
                status,
                error_code,
                detail,
            } => match status {
                401 => OagwError::AuthFailed(format!(
                    "plugin '{plugin_ref}' rejected the request ({error_code}): {detail}"
                )),
                _ => {
                    if error_code.eq_ignore_ascii_case("REQUIRED_HEADER_MISSING") && status == 502 {
                        OagwError::DownstreamError(format!(
                            "plugin '{plugin_ref}' rejected the upstream response \
                             ({error_code}): {detail}"
                        ))
                    } else if error_code.eq_ignore_ascii_case("REQUIRED_HEADER_MISSING") {
                        OagwError::Validation(format!(
                            "plugin '{plugin_ref}' rejected the request ({error_code}): {detail}"
                        ))
                    } else {
                        OagwError::Validation(format!(
                            "plugin '{plugin_ref}' rejected the request ({error_code}): {detail}"
                        ))
                    }
                }
            },
            PluginError::Internal(msg) => {
                OagwError::Internal(format!("plugin '{plugin_ref}' failed: {msg}"))
            }
            PluginError::Unknown(msg) => OagwError::PluginNotFound(msg),
        }
    }

    /// Reject a request-phase guard whose error code is `REQUIRED_HEADER_MISSING`
    /// (ADR-0009: request phase 400).
    pub fn required_header_missing_request(detail: String) -> Self {
        Self::Reject {
            status: 400,
            error_code: "REQUIRED_HEADER_MISSING".to_owned(),
            detail,
        }
    }

    /// Reject a response-phase guard missing header (ADR-0009: 502).
    pub fn required_header_missing_response(detail: String) -> Self {
        Self::Reject {
            status: 502,
            error_code: "REQUIRED_HEADER_MISSING".to_owned(),
            detail,
        }
    }
}

/// Context handed to auth plugins.
pub struct AuthContext {
    /// Opaque plugin configuration from the upstream binding.
    pub config: serde_json::Value,
    /// The outbound request headers collected so far (plugins mutate these).
    pub headers: HeaderMap,
    /// Query parameters to append to the outbound URL (e.g. API key in
    /// query mode).
    pub query_params: Vec<(String, String)>,
    /// Tenant/subject context of the inbound caller (fully populated by the
    /// api-gateway auth middleware).
    pub security_context: SecurityContext,
    /// Credential store for `cred://` secret resolution.
    pub credstore: Arc<dyn CredStoreClientV1>,
}

impl std::fmt::Debug for AuthContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Redact headers (may carry injected credentials) and config.
        f.debug_struct("AuthContext")
            .field("config", &"[redacted]")
            .field("headers", &"[redacted]")
            .field("query_params", &self.query_params)
            .field("security_context", &self.security_context)
            .field("credstore", &"[credstore]")
            .finish()
    }
}

/// Context handed to guard plugins.
#[derive(Debug, Clone)]
pub struct GuardContext {
    pub headers: HeaderMap,
    /// `true` when running the response phase.
    pub is_response: bool,
    pub config: serde_json::Value,
}

/// Context handed to transform plugins.
#[derive(Debug, Clone)]
pub struct TransformContext {
    pub config: serde_json::Value,
    pub headers: HeaderMap,
}

/// Authentication plugin: injects credentials into the outbound request.
#[async_trait]
pub trait AuthPlugin: Send + Sync {
    /// Canonical GTS identifier of the plugin.
    fn id(&self) -> &str;

    /// Inject credentials into `ctx` (or reject).
    async fn authenticate(&self, ctx: &mut AuthContext) -> Result<(), PluginError>;
}

/// Guard plugin: enforces policies on the request and/or response phase.
#[async_trait]
pub trait GuardPlugin: Send + Sync {
    /// Validate the outbound request before it is forwarded.
    async fn guard_request(&self, ctx: &GuardContext) -> Result<(), PluginError>;

    /// Validate the upstream response before it is returned.
    async fn guard_response(&self, ctx: &GuardContext) -> Result<(), PluginError>;
}

/// Transform plugin: mutates request or response data.
#[async_trait]
pub trait TransformPlugin: Send + Sync {
    /// Mutate the outbound request.
    async fn transform_request(&self, ctx: &mut TransformContext) -> Result<(), PluginError>;

    /// Mutate the upstream response.
    async fn transform_response(&self, ctx: &mut TransformContext) -> Result<(), PluginError>;
}

/// Convenience: parse `Method` into the schema's method-string set.
#[must_use]
pub fn method_to_str(method: &Method) -> &'static str {
    match *method {
        Method::GET => "GET",
        Method::POST => "POST",
        Method::PUT => "PUT",
        Method::DELETE => "DELETE",
        Method::PATCH => "PATCH",
        Method::HEAD => "HEAD",
        Method::OPTIONS => "OPTIONS",
        _ => "OTHER",
    }
}
