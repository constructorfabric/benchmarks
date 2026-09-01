//! Plugin system traits and registries (ADR-0002).
//!
//! Three plugin types with separate traits, executed in a deterministic order:
//! Auth → Guards → Transform(on_request) → upstream call →
//! Transform(on_response). Upstream plugins execute before route plugins.
//!
//! Plugins only see and mutate the request/response *parts* (status line,
//! method, URI, headers, extensions) — bodies are never materialised, so a
//! plugin cannot turn a streaming proxy into a buffered one.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::domain::error::DomainError;

/// A resolved plugin binding: the reference plus its effective config.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PluginInvocation {
    /// Plugin reference (GTS identifier or UUID).
    pub plugin_ref: String,
    /// Plugin configuration JSON.
    pub config: serde_json::Value,
    /// Whether the plugin came from the upstream (vs the route) chain.
    pub from_upstream: bool,
}

/// Context handed to plugins, spanning the whole request lifetime.
#[derive(Debug, Clone)]
pub struct PluginContext {
    /// Authenticated caller.
    pub security_context: toolkit_security::SecurityContext,
    /// Selected upstream.
    pub upstream_id: uuid::Uuid,
    /// Upstream alias (metric/log label).
    pub host: String,
    /// Matched route id, when a route matched.
    pub route_id: Option<uuid::Uuid>,
    /// Resolved upstream endpoint authority (`host:port`).
    pub endpoint_host: String,
    /// Correlation identifier of this request: the inbound `X-Request-ID` when
    /// the caller supplied one, a freshly minted `oagw-{uuid}` otherwise. Every
    /// consumer (audit line, `X-Request-ID` transform) reads this one value.
    pub request_id: String,
}

impl PluginContext {
    /// `request_id` for tracing and the `X-Request-ID` transform plugin.
    #[must_use]
    pub fn request_id(&self) -> &str {
        &self.request_id
    }

    /// Generates a fresh correlation identifier without a context.
    #[must_use]
    pub fn default_request_id() -> String {
        format!("oagw-{}", uuid::Uuid::new_v4())
    }

    /// Resolves the request's correlation identifier.
    ///
    /// An inbound `X-Request-ID` (or the `header` alternative configured on the
    /// request-id transform) is propagated verbatim so the caller can correlate
    /// the audit line with its own logs; a blank, malformed or absent header
    /// yields a freshly minted identifier.
    #[must_use]
    pub fn request_id_from_headers(headers: &http::HeaderMap) -> String {
        headers
            .get(REQUEST_ID_HEADER)
            .and_then(|value| value.to_str().ok())
            .map(str::trim)
            .filter(|value| !value.is_empty() && value.len() <= MAX_REQUEST_ID_LEN)
            .map_or_else(Self::default_request_id, str::to_owned)
    }
}

/// Header the correlation identifier is read from.
pub const REQUEST_ID_HEADER: &str = "x-request-id";
/// Upper bound on an accepted inbound correlation identifier.
pub const MAX_REQUEST_ID_LEN: usize = 256;

/// Credential-injection plugin. Exactly one per upstream, executed first.
#[async_trait]
pub trait AuthPlugin: Send + Sync {
    /// The plugin identifier this implementation answers to (the GTS
    /// identifier's instance part, e.g. `cf.core.oagw.apikey.v1`).
    fn id(&self) -> &'static str;

    /// Injects credentials into the outbound request.
    ///
    /// # Errors
    ///
    /// Returns 401 [`DomainError::AuthenticationFailed`] or 500
    /// [`DomainError::SecretNotFound`].
    async fn authenticate(
        &self,
        ctx: &PluginContext,
        security_context: &toolkit_security::SecurityContext,
        config: &serde_json::Value,
        parts: &mut http::request::Parts,
    ) -> Result<(), DomainError>;
}

/// Validation / policy plugin. Many per upstream or route, executed after
/// auth and before transforms. Can reject the request or a response.
#[async_trait]
pub trait GuardPlugin: Send + Sync {
    /// The plugin identifier (instance part of the GTS identifier).
    fn id(&self) -> &'static str;

    /// Validates the outbound request.
    ///
    /// # Errors
    ///
    /// Returns a 4xx [`DomainError`] to reject the request.
    async fn guard_request(
        &self,
        ctx: &PluginContext,
        config: &serde_json::Value,
        parts: &http::request::Parts,
    ) -> Result<(), DomainError>;

    /// Validates the upstream response before it is relayed.
    ///
    /// # Errors
    ///
    /// Returns a [`DomainError`] to replace the response with a problem.
    async fn guard_response(
        &self,
        ctx: &PluginContext,
        config: &serde_json::Value,
        parts: &http::response::Parts,
    ) -> Result<(), DomainError>;
}

/// Request / response mutation plugin. Many per upstream or route.
#[async_trait]
pub trait TransformPlugin: Send + Sync {
    /// The plugin reference's instance part.
    fn id(&self) -> &'static str;

    /// Mutates the outbound request parts.
    ///
    /// # Errors
    ///
    /// Returns a [`DomainError`] to abort the request.
    async fn transform_request(
        &self,
        ctx: &PluginContext,
        config: &serde_json::Value,
        parts: &mut http::request::Parts,
    ) -> Result<(), DomainError>;

    /// Mutates the relayed response parts.
    ///
    /// # Errors
    ///
    /// Returns a [`DomainError`] to abort the relay.
    async fn transform_response(
        &self,
        ctx: &PluginContext,
        config: &serde_json::Value,
        parts: &mut http::response::Parts,
    ) -> Result<(), DomainError>;
}

/// Auth plugin registry.
pub struct AuthPluginRegistry {
    plugins: std::collections::BTreeMap<String, std::sync::Arc<dyn AuthPlugin>>,
}

impl AuthPluginRegistry {
    /// Builds a registry from the supplied plugins.
    #[must_use]
    pub fn new(plugins: Vec<std::sync::Arc<dyn AuthPlugin>>) -> Self {
        let plugins = plugins
            .into_iter()
            .map(|plugin| (plugin.id().to_owned(), plugin))
            .collect();
        Self { plugins }
    }

    /// Resolves a plugin reference (GTS identifier or instance part).
    ///
    /// # Errors
    ///
    /// Returns 503 [`DomainError::PluginNotFound`] when the reference is
    /// unresolvable.
    pub fn resolve(&self, plugin_ref: &str) -> Result<std::sync::Arc<dyn AuthPlugin>, DomainError> {
        let instance = crate::domain::gts::plugin_ref_instance(plugin_ref);
        self.plugins.get(instance).cloned().ok_or_else(|| {
            DomainError::PluginNotFound(format!("unknown auth plugin '{plugin_ref}'"))
        })
    }

    /// All registered plugin ids.
    #[must_use]
    pub fn ids(&self) -> Vec<String> {
        self.plugins.keys().cloned().collect()
    }
}

/// Guard plugin registry.
pub struct GuardPluginRegistry {
    plugins: std::collections::BTreeMap<String, std::sync::Arc<dyn GuardPlugin>>,
}

impl GuardPluginRegistry {
    /// All registered plugin ids.
    #[must_use]
    pub fn ids(&self) -> Vec<String> {
        self.plugins.keys().cloned().collect()
    }

    /// Builds a registry from the supplied plugins.
    #[must_use]
    pub fn new(plugins: Vec<std::sync::Arc<dyn GuardPlugin>>) -> Self {
        let plugins = plugins
            .into_iter()
            .map(|plugin| (plugin.id().to_owned(), plugin))
            .collect();
        Self { plugins }
    }

    /// Resolves a guard plugin reference.
    ///
    /// # Errors
    ///
    /// Returns 503 [`DomainError::PluginNotFound`] when unresolvable.
    pub fn resolve(
        &self,
        plugin_ref: &str,
    ) -> Result<std::sync::Arc<dyn GuardPlugin>, DomainError> {
        let instance = crate::domain::gts::plugin_ref_instance(plugin_ref);
        self.plugins.get(instance).cloned().ok_or_else(|| {
            DomainError::PluginNotFound(format!("unknown guard plugin '{plugin_ref}'"))
        })
    }
}

/// Transform plugin registry.
pub struct TransformPluginRegistry {
    plugins: std::collections::BTreeMap<String, std::sync::Arc<dyn TransformPlugin>>,
}

impl TransformPluginRegistry {
    /// Builds a registry from the supplied plugins.
    #[must_use]
    pub fn new(plugins: Vec<std::sync::Arc<dyn TransformPlugin>>) -> Self {
        let plugins = plugins
            .into_iter()
            .map(|plugin| (plugin.id().to_owned(), plugin))
            .collect();
        Self { plugins }
    }

    /// Resolves a transform plugin reference.
    ///
    /// # Errors
    ///
    /// Returns 503 [`DomainError::PluginNotFound`] when unresolvable.
    pub fn resolve(
        &self,
        plugin_ref: &str,
    ) -> Result<std::sync::Arc<dyn TransformPlugin>, DomainError> {
        let instance = crate::domain::gts::plugin_ref_instance(plugin_ref);
        self.plugins.get(instance).cloned().ok_or_else(|| {
            DomainError::PluginNotFound(format!("unknown transform plugin '{plugin_ref}'"))
        })
    }
}

/// A resolved, ordered plugin chain for one proxy request.
#[derive(Debug, Clone, Default)]
pub struct PluginChain {
    /// Auth plugin binding, if the upstream configures one.
    pub auth: Option<PluginInvocation>,
    /// Ordered guard bindings (upstream before route).
    pub guards: Vec<PluginInvocation>,
    /// Ordered transform bindings.
    pub transforms: Vec<PluginInvocation>,
}

/// Parses a `cred://` URI reference into a [`credstore_sdk::SecretRef`].
///
/// # Errors
///
/// Returns 500 [`DomainError::SecretNotFound`] when the reference is empty or
/// malformed.
pub fn parse_secret_ref(reference: &str) -> Result<credstore_sdk::SecretRef, DomainError> {
    let key = reference
        .trim()
        .strip_prefix("cred://")
        .unwrap_or_else(|| reference.trim());
    credstore_sdk::SecretRef::new(key).map_err(|_| {
        DomainError::SecretNotFound(format!(
            "secret reference '{reference}' is not a valid cred:// key"
        ))
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    #[test]
    fn request_id_is_taken_from_the_inbound_header() {
        let mut headers = http::HeaderMap::new();
        assert!(PluginContext::request_id_from_headers(&headers).starts_with("oagw-"));
        headers.insert(
            http::header::HeaderName::from_static("x-request-id"),
            http::HeaderValue::from_static("abc-123"),
        );
        assert_eq!(PluginContext::request_id_from_headers(&headers), "abc-123");
        headers.insert(
            http::header::HeaderName::from_static("x-request-id"),
            http::HeaderValue::from_static("   "),
        );
        assert!(PluginContext::request_id_from_headers(&headers).starts_with("oagw-"));
    }

    #[test]
    fn request_id_returns_the_context_value() {
        let context = PluginContext {
            security_context: toolkit_security::SecurityContext::anonymous(),
            upstream_id: uuid::Uuid::nil(),
            host: "vendor.com".to_owned(),
            route_id: None,
            endpoint_host: "api.vendor.com:443".to_owned(),
            request_id: "abc-123".to_owned(),
        };
        assert_eq!(context.request_id(), "abc-123");
    }

    #[test]
    fn secret_ref_strips_scheme() {
        let parsed = parse_secret_ref("cred://partner-openai-key").unwrap();
        assert_eq!(parsed.as_ref(), "partner-openai-key");
        let bare = parse_secret_ref("partner-openai-key").unwrap();
        assert_eq!(bare.as_ref(), "partner-openai-key");
        assert!(parse_secret_ref("cred://not allowed").is_err());
        assert!(parse_secret_ref("").is_err());
    }

    #[test]
    fn registries_resolve_by_instance_part() {
        struct NoopAuth;
        #[async_trait]
        impl AuthPlugin for NoopAuth {
            fn id(&self) -> &'static str {
                "cf.core.oagw.noop.v1"
            }
            async fn authenticate(
                &self,
                _ctx: &PluginContext,
                _security_context: &toolkit_security::SecurityContext,
                _config: &serde_json::Value,
                _parts: &mut http::request::Parts,
            ) -> Result<(), DomainError> {
                Ok(())
            }
        }
        let registry = AuthPluginRegistry::new(vec![std::sync::Arc::new(NoopAuth)]);
        assert!(
            registry
                .resolve(crate::domain::gts::AUTH_PLUGIN_NOOP)
                .is_ok()
        );
        let err = registry
            .resolve(crate::domain::gts::AUTH_PLUGIN_BASIC)
            .err();
        assert!(matches!(err, Some(DomainError::PluginNotFound(_))));
    }
}
