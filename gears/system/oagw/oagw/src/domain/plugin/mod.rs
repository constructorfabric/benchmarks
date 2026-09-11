//! Plugin traits.
//!
//! Three families, executed in the order authentication → guards →
//! transform(request) → upstream call → transform(response/error). A plugin
//! sees the resolved configuration and mutates headers only; it never owns the
//! body or the connection.

use async_trait::async_trait;
use uuid::Uuid;

use crate::domain::error::DomainError;

/// Execution context handed to every plugin.
#[derive(Debug, Clone)]
pub struct PluginContext {
    /// Owning tenant.
    pub tenant_id: Uuid,
    /// Authenticated subject, or nil for an anonymous call.
    pub subject_id: Uuid,
    /// Resolved upstream.
    pub upstream_id: Uuid,
    /// Matched route, when one was matched.
    pub route_id: Option<Uuid>,
    /// The alias the request addressed.
    pub alias: String,
    /// The bearer token on the inbound request, if any.
    pub bearer_token: Option<String>,
    /// The correlation id the request phase settled on, propagated or minted.
    ///
    /// The response phase echoes it, so the caller can correlate a reply with
    /// the request it belongs to without inspecting the upstream's headers.
    pub request_id: Option<String>,
}

impl PluginContext {
    /// Cache namespace for per-tenant, per-subject artefacts such as `OAuth2`
    /// tokens.
    #[must_use]
    pub fn cache_scope(&self) -> String {
        format!("{}|{}", self.tenant_id, self.subject_id)
    }
}

/// Resolves credential material at request time, by reference.
#[async_trait]
pub trait SecretResolver: Send + Sync {
    /// Resolves a secret reference to its value.
    ///
    /// # Errors
    /// Returns [`DomainError::SecretNotFound`] when the store cannot resolve
    /// the reference, so callers can distinguish a missing secret from an
    /// unusable one.
    async fn resolve(
        &self,
        context: &PluginContext,
        reference: &str,
    ) -> Result<Option<String>, DomainError>;
}

/// A no-op resolver used when no credential store is wired in.
#[derive(Debug, Default)]
pub struct NullSecretResolver;

#[async_trait]
impl SecretResolver for NullSecretResolver {
    async fn resolve(
        &self,
        _context: &PluginContext,
        _reference: &str,
    ) -> Result<Option<String>, DomainError> {
        Ok(None)
    }
}

/// Injects credentials into an outbound request.
#[async_trait]
pub trait AuthPlugin: Send + Sync {
    /// The plugin's GTS identifier.
    fn id(&self) -> &'static str;

    /// Applies credentials to `headers`.
    ///
    /// # Errors
    /// Returns [`DomainError::AuthenticationFailed`] when credentials cannot be
    /// resolved or the configuration is unusable.
    async fn apply(
        &self,
        context: &PluginContext,
        config: &serde_json::Value,
        headers: &mut http::HeaderMap,
        query: &mut Vec<(String, String)>,
    ) -> Result<(), DomainError>;
}

/// Checks a request or response against a policy without transforming it.
#[async_trait]
pub trait GuardPlugin: Send + Sync {
    /// The plugin's GTS identifier.
    fn id(&self) -> &'static str;

    /// Validates the inbound request.
    ///
    /// Guards judge rather than rewrite, so this sees the caller's headers as
    /// they arrived, before the header rules select what the upstream receives.
    ///
    /// # Errors
    /// Returns a `400`-family [`DomainError`] when the request must be refused.
    async fn guard_request(
        &self,
        context: &PluginContext,
        config: &serde_json::Value,
        headers: &http::HeaderMap,
    ) -> Result<(), DomainError>;

    /// Validates the upstream response.
    ///
    /// Guards see the response as the upstream sent it, before the gateway's
    /// own header rules run, so a header the gateway adds for the caller does
    /// not satisfy a check on the upstream's behaviour.
    ///
    /// # Errors
    /// Returns a `502`-family [`DomainError`] when the response must be
    /// replaced by a gateway error.
    async fn guard_response(
        &self,
        context: &PluginContext,
        config: &serde_json::Value,
        status: http::StatusCode,
        headers: &mut http::HeaderMap,
    ) -> Result<(), DomainError>;
}

/// Rewrites a request or a response.
#[async_trait]
pub trait TransformPlugin: Send + Sync {
    /// The plugin's GTS identifier.
    fn id(&self) -> &'static str;

    /// Rewrites the outbound request.
    ///
    /// `inbound` is what the caller sent, untouched by the header rules, so a
    /// transform that propagates a caller-supplied value can still read it once
    /// the rules have dropped it from what the upstream receives.
    ///
    /// # Errors
    /// Returns a [`DomainError`] when the request cannot be transformed.
    async fn transform_request(
        &self,
        context: &PluginContext,
        config: &serde_json::Value,
        inbound: &http::HeaderMap,
        headers: &mut http::HeaderMap,
    ) -> Result<(), DomainError>;

    /// Rewrites the response seen by the client.
    ///
    /// # Errors
    /// Returns a [`DomainError`] when the response cannot be transformed.
    async fn transform_response(
        &self,
        context: &PluginContext,
        config: &serde_json::Value,
        status: http::StatusCode,
        headers: &mut http::HeaderMap,
    ) -> Result<(), DomainError>;
}
