//! Plugin contracts, context types and the built-in plugins.
//!
//! The chain runs Auth → Guards → Transform(`on_request`) → upstream →
//! Transform(`on_response` | `on_error`); upstream items always run before
//! route items.

use crate::domain::error::OagwError;
use crate::domain::model::AuthConfig;
use async_trait::async_trait;
use bytes::Bytes;
use http::HeaderMap;
use uuid::Uuid;

pub mod apikey;
pub mod noop;
pub mod oauth2_client_cred;
pub mod registry;
pub mod request_id;
pub mod required_headers_guard;

/// Credential material, held out of logs, error messages and debug output.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct Credential(Vec<u8>);

impl Credential {
    /// Wraps raw credential bytes.
    #[must_use]
    pub fn new(bytes: Vec<u8>) -> Self {
        Self(bytes)
    }

    /// The credential bytes. Only the plugin that requested them sees these.
    #[must_use]
    pub fn expose(&self) -> &[u8] {
        &self.0
    }

    /// The credential as UTF-8, when it is text.
    #[must_use]
    pub fn as_text(&self) -> Option<&str> {
        std::str::from_utf8(&self.0).ok()
    }
}

impl std::fmt::Debug for Credential {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("Credential(***)")
    }
}

/// Resolves `cred://` references on behalf of a plugin.
#[async_trait]
pub trait CredentialResolver: Send + Sync {
    /// Reads the secret a reference names.
    ///
    /// # Errors
    ///
    /// Returns [`OagwError::AuthenticationFailed`] when the secret is
    /// inaccessible and [`OagwError::SecretNotFound`] when it does not exist.
    async fn resolve(&self, tenant_id: Uuid, reference: &str) -> Result<Credential, OagwError>;
}

/// Mutable state carried through the request phase of the chain.
#[derive(Debug)]
pub struct PluginRequestContext {
    /// Correlation identifier.
    pub request_id: String,
    /// Owning tenant.
    pub tenant_id: Uuid,
    /// Resolved upstream identifier.
    pub upstream_id: String,
    /// Matched route identifier, when a route matched.
    pub route_id: Option<String>,
    /// Upstream alias.
    pub alias: String,
    /// Selected endpoint host.
    pub target_host: String,
    /// Request method.
    pub method: String,
    /// Upstream path.
    pub path: String,
    /// Raw query string.
    pub query: String,
    /// Outbound headers, mutated in place.
    pub headers: HeaderMap,
    /// Request body.
    pub body: Bytes,
    /// Credential injected by the auth phase, if any.
    pub credential: Option<Credential>,
}

/// Mutable state carried through the response phase of the chain.
#[derive(Debug)]
pub struct PluginResponseContext {
    /// Correlation identifier.
    pub request_id: String,
    /// Upstream status code.
    pub status: u16,
    /// Response headers, mutated in place.
    pub headers: HeaderMap,
}

/// Outcome of the auth phase.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthDecision {
    /// The plugin injected credentials.
    Injected,
    /// The plugin requires no credentials for this request.
    Passthrough,
}

/// Outcome of a guard phase.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GuardDecision {
    /// The check passed.
    Allow,
    /// The check failed; the request or response is rejected.
    Reject {
        /// Status to return.
        status: u16,
        /// Machine-readable reason.
        code: &'static str,
        /// Human-readable detail naming at most the first violation.
        detail: String,
    },
}

/// Credential-injection plugin.
#[async_trait]
pub trait AuthPlugin: Send + Sync {
    /// The plugin's GTS identifier.
    fn id(&self) -> &str;

    /// Injects credentials into the outbound request.
    ///
    /// # Errors
    ///
    /// Returns [`OagwError::AuthenticationFailed`] or
    /// [`OagwError::SecretNotFound`] when credentials cannot be resolved.
    async fn authenticate(
        &self,
        context: &mut PluginRequestContext,
        config: &AuthConfig,
        credentials: &dyn CredentialResolver,
    ) -> Result<AuthDecision, OagwError>;
}

/// Validation and policy plugin.
#[async_trait]
pub trait GuardPlugin: Send + Sync {
    /// The plugin's GTS identifier.
    fn id(&self) -> &str;

    /// Runs before the upstream call.
    ///
    /// # Errors
    ///
    /// Returns [`OagwError::ValidationError`] when the check cannot run.
    async fn guard_request(
        &self,
        context: &PluginRequestContext,
        config: &serde_json::Value,
    ) -> Result<GuardDecision, OagwError>;

    /// Runs on the upstream response.
    ///
    /// # Errors
    ///
    /// Returns [`OagwError::ValidationError`] when the check cannot run.
    async fn guard_response(
        &self,
        context: &PluginResponseContext,
        config: &serde_json::Value,
    ) -> Result<GuardDecision, OagwError>;
}

/// Request and response mutation plugin.
#[async_trait]
pub trait TransformPlugin: Send + Sync {
    /// The plugin's GTS identifier.
    fn id(&self) -> &str;

    /// Runs before the upstream call.
    ///
    /// # Errors
    ///
    /// Returns [`OagwError::ValidationError`] when the transform fails.
    async fn transform_request(
        &self,
        context: &mut PluginRequestContext,
        config: &serde_json::Value,
    ) -> Result<(), OagwError>;

    /// Runs on the upstream response.
    ///
    /// # Errors
    ///
    /// Returns [`OagwError::ValidationError`] when the transform fails.
    async fn transform_response(
        &self,
        context: &mut PluginResponseContext,
        config: &serde_json::Value,
    ) -> Result<(), OagwError>;

    /// Runs when the upstream call fails.
    ///
    /// # Errors
    ///
    /// Returns [`OagwError::ValidationError`] when the transform fails.
    async fn transform_error(
        &self,
        context: &mut PluginResponseContext,
        config: &serde_json::Value,
    ) -> Result<(), OagwError> {
        let _ = (context, config);
        Ok(())
    }
}

/// Reads a string configuration value.
#[must_use]
pub fn config_string(config: &serde_json::Value, key: &str) -> Option<String> {
    config
        .get(key)
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
}

/// Reads a numeric configuration value.
#[must_use]
pub fn config_u64(config: &serde_json::Value, key: &str) -> Option<u64> {
    config.get(key).and_then(serde_json::Value::as_u64)
}

/// Splits a comma-separated configuration list, dropping empty entries.
#[must_use]
pub fn config_list(config: &serde_json::Value, key: &str) -> Vec<String> {
    config_string(config, key)
        .map(|raw| {
            raw.split(',')
                .map(str::trim)
                .filter(|entry| !entry.is_empty())
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
pub(crate) mod test_support;
