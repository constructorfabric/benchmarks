//! Plugin contracts for the OAGW data plane.
//!
//! Three plugin kinds mirror `DOCS §3.1` (`ADR 0002` — verbatim trait
//! shapes): [`AuthPlugin`] (credential injection, exactly one per
//! upstream, runs first), [`GuardPlugin`] (validation/policy, can reject
//! requests), [`TransformPlugin`] (request/response/error mutation).
//!
//! Plugins are resolved from in-process registries (`infra::plugins`)
//! keyed by their well-known GTS instance id; binding a catalog-only id
//! fails during control-plane validation or proxy resolution with
//! "unknown plugin" (DOCS §3.3).
//!
//! All plugin work is async and header-oriented: the proxy pipeline owns
//! the request/response headers and threads them through plugin hooks.

use async_trait::async_trait;
use credstore_sdk::CredStoreClientV1;
use http::header::HeaderMap;
use std::sync::Arc;

use toolkit_security::SecurityContext;

use crate::domain::error::DataPlaneError;

/// Environment every plugin hook can rely on: who is proxying, where the
/// upstream config came from, and the sibling gear clients a plugin may
/// consult (credential store, HTTP client for token exchange).
#[derive(Clone)]
pub struct PluginContext {
    /// Security context of the proxying principal (plugins resolve
    /// `cred://` references under the caller's identity).
    pub security_context: SecurityContext,
    /// Credential store client for `cred://` reference resolution.
    pub cred_store: Arc<dyn CredStoreClientV1>,
    /// HTTP client used by plugins that need to talk to an `OAuth2` token
    /// endpoint.
    pub http: toolkit_http::HttpClient,
    /// Effective configuration for the binding site (plugin `config`
    /// merged over the plugin definition config).
    pub config: serde_json::Value,
}

/// Plugin failure — maps onto a data-plane problem response.
#[derive(Debug, Clone, thiserror::Error)]
pub enum PluginError {
    /// Missing or unresolvable secret reference.
    #[error("secret not found: {0}")]
    SecretNotFound(String),
    /// Upstream authentication failed (401 `cf.oagw.auth.failed`).
    #[error("authentication failed: {0}")]
    AuthFailed(String),
    /// Plugin implementation not registered / not bindable.
    #[error("plugin not found: {0}")]
    PluginNotFound(String),
    /// `OAuth2` token endpoint unreachable or returned an unusable token.
    #[error("token acquisition failed: {0}")]
    TokenAcquisitionFailed(String),
    /// Generic internal failure (500).
    #[error("plugin internal error: {0}")]
    Internal(String),
}

impl PluginError {
    /// Map to a data-plane error (all failures here are 500-style except
    /// auth/secret which surface as 401/500 per DOCS §8).
    #[must_use]
    pub fn into_data_plane(self, extensions: crate::domain::error::ErrorExtensions) -> DataPlaneError {
        match self {
            Self::SecretNotFound(detail) => DataPlaneError::SecretNotFound { detail, extensions },
            Self::AuthFailed(detail) => DataPlaneError::AuthFailed { detail, extensions },
            Self::PluginNotFound(detail) => DataPlaneError::PluginNotFound { detail, extensions },
            Self::TokenAcquisitionFailed(detail) => {
                DataPlaneError::AuthFailed { detail, extensions }
            }
            Self::Internal(detail) => DataPlaneError::Internal { detail, extensions },
        }
    }
}

/// Outcome of a guard check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GuardDecision {
    /// Proceed with the request.
    Continue,
    /// Reject with the given status code and problem detail. The error
    /// code surface (`REQUIRED_HEADER_MISSING`) is carried as detail.
    Reject { status: u16, detail: String },
}

/// Auth plugin: injects credentials into the outbound request headers.
#[async_trait]
pub trait AuthPlugin: Send + Sync {
    /// Plugin identity (GTS instance id).
    fn id(&self) -> &'static str;

    /// Plugin kind (`auth`).
    fn plugin_type(&self) -> &'static str;

    /// Authenticate and inject credentials into `headers`.
    ///
    /// # Errors
    /// Returns `PluginError` when credential resolution or injection fails.
    async fn authenticate(&self, ctx: &PluginContext, headers: &mut HeaderMap) -> Result<(), PluginError>;
}

/// Guard plugin: validates a request (or response) and may reject it.
#[async_trait]
pub trait GuardPlugin: Send + Sync {
    /// Plugin identity (GTS instance id).
    fn id(&self) -> &'static str;

    /// Plugin kind (`guard`).
    fn plugin_type(&self) -> &'static str;

    /// Validate the client's request headers before proxying (ADR 0009:
    /// header validation is independent of the forwarding/passthrough
    /// config, so guards inspect the inbound set).
    ///
    /// # Errors
    /// Returns `PluginError` on internal failure; policy failures are
    /// expressed via [`GuardDecision::Reject`].
    async fn guard_request(&self, ctx: &PluginContext, headers: &HeaderMap) -> Result<GuardDecision, PluginError>;

    /// Validate the upstream response before it is returned to the client.
    ///
    /// # Errors
    /// Returns `PluginError` on internal failure; policy failures are
    /// expressed via [`GuardDecision::Reject`].
    async fn guard_response(&self, ctx: &PluginContext, headers: &HeaderMap) -> Result<GuardDecision, PluginError>;
}

/// Transform plugin: mutates the request and/or response.
#[async_trait]
pub trait TransformPlugin: Send + Sync {
    /// Plugin identity (GTS instance id).
    fn id(&self) -> &'static str;

    /// Plugin kind (`transform`).
    fn plugin_type(&self) -> &'static str;

    /// Mutation phase on the outbound request (after auth, before send).
    ///
    /// # Errors
    /// Returns `PluginError` on failure.
    async fn transform_request(&self, ctx: &PluginContext, headers: &mut HeaderMap) -> Result<(), PluginError>;

    /// Mutation phase on the upstream response (before returning).
    ///
    /// # Errors
    /// Returns `PluginError` on failure.
    async fn transform_response(&self, ctx: &PluginContext, headers: &mut HeaderMap) -> Result<(), PluginError>;
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn plugin_error_maps_to_documented_statuses() {
        let dpe = PluginError::AuthFailed("bad creds".into())
            .into_data_plane(crate::domain::error::ErrorExtensions::default());
        assert_eq!(dpe.status(), 401);
        assert_eq!(dpe.error_type(), crate::gts_helpers::ERR_AUTH_FAILED);
    }
}
