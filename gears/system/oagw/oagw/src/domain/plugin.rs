// Created: 2026-09-02 by Constructor Tech
//! Plugin contracts and registries (`ADR-0002`).
//!
//! Three plugin kinds run in a fixed order around the upstream call:
//!
//! ```text
//! auth → guards (request) → transforms (request) → upstream → transforms (response/error)
//! ```
//!
//! Upstream plugins run before route plugins. Built-in plugins are resolved
//! through the in-process registries here; tenant-defined custom plugins are
//! resolved by id from the store and are Starlark sources, which this gateway
//! persists and serves (`GET /plugins/{id}/source`) without executing.
//!
//! The trait shapes below are the ones the ADR describes, narrowed to what the
//! data plane actually needs: an auth plugin returns the credentials to inject,
//! a guard plugin returns `Allow`/`Reject`, and a transform plugin mutates the
//! request/response head.

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::http::{HeaderName, HeaderValue};
use uuid::Uuid;

use crate::error::GatewayError;

/// Resolves a `cred://` reference to secret material for a tenant.
#[async_trait::async_trait]
pub trait SecretResolver: Send + Sync {
    /// Resolves `reference` (with or without the `cred://` prefix).
    ///
    /// `Ok(None)` means the reference exists but is not readable by the tenant;
    /// `Err` means the credential store itself failed.
    async fn resolve(
        &self,
        tenant_id: Uuid,
        reference: &str,
    ) -> anyhow::Result<Option<String>>;
}

/// Credentials an auth plugin injects into the outbound request.
#[derive(Debug, Clone, Default)]
pub struct AuthInjection {
    /// Headers to set on the outbound request.
    pub headers: Vec<(HeaderName, HeaderValue)>,
    /// Query parameters to append to the outbound URL.
    pub query: Vec<(String, String)>,
}

impl AuthInjection {
    /// An injection that adds nothing.
    #[must_use]
    pub fn none() -> Self {
        Self::default()
    }

    /// Builds a header injection from a name/value pair.
    ///
    /// Invalid names or values are dropped (they cannot be rendered on the wire
    /// anyway) rather than failing the request.
    #[must_use]
    pub fn header(name: &str, value: &str) -> Option<(HeaderName, HeaderValue)> {
        let name = HeaderName::try_from(name).ok()?;
        let value = HeaderValue::try_from(value).ok()?;
        Some((name, value))
    }

    /// Whether anything is injected at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.headers.is_empty() && self.query.is_empty()
    }
}

/// Inputs an auth plugin sees.
#[derive(Clone, Copy)]
pub struct AuthContext<'a> {
    /// Owning tenant of the upstream (the secret's tenant context).
    pub tenant_id: Uuid,
    /// Calling subject's tenant, for cache isolation (ADR-0008).
    pub subject_tenant_id: Uuid,
    /// Calling subject id, for cache isolation (ADR-0008).
    pub subject_id: &'a str,
    /// Upstream GTS instance UUID.
    pub upstream_id: Uuid,
    /// The upstream's `auth.config` object.
    pub config: &'a BTreeMap<String, serde_json::Value>,
    /// Credential store.
    pub secrets: &'a dyn SecretResolver,
}

impl<'a> std::fmt::Debug for AuthContext<'a> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Secrets are behind a trait object with no `Debug`; the context is
        // logged only by its coordinates, never by its contents.
        f.debug_struct("AuthContext")
            .field("tenant_id", &self.tenant_id)
            .field("subject_tenant_id", &self.subject_tenant_id)
            .field("subject_id", &self.subject_id)
            .field("upstream_id", &self.upstream_id)
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

impl<'a> AuthContext<'a> {
    /// Configuration value for `key`, coerced to a string.
    #[must_use]
    pub fn config_str(&self, key: &str) -> Option<String> {
        match self.config.get(key)? {
            serde_json::Value::String(s) => Some(s.clone()),
            serde_json::Value::Number(n) => Some(n.to_string()),
            serde_json::Value::Bool(b) => Some(b.to_string()),
            _ => None,
        }
    }
}

/// A built-in auth plugin.
#[async_trait::async_trait]
pub trait AuthPlugin: Send + Sync {
    /// GTS identifier this plugin resolves.
    fn plugin_id(&self) -> &'static str;

    /// Computes the credentials to inject.
    ///
    /// # Errors
    ///
    /// [`GatewayError::Validation`] for a malformed binding,
    /// [`GatewayError::AuthenticationFailed`] when credentials cannot be
    /// resolved or the IdP rejects them.
    async fn authenticate(&self, ctx: &AuthContext<'_>) -> Result<AuthInjection, GatewayError>;
}

/// Verdict of a guard plugin.
#[derive(Debug, Clone)]
pub enum GuardVerdict {
    /// Continue the chain.
    Allow,
    /// Stop the chain and return this error to the caller.
    Reject(GatewayError),
}

impl PartialEq for GuardVerdict {
    // Two verdicts are the same decision when both continue or both stop with
    // the same status code; the problem detail text is not part of the contract.
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Allow, Self::Allow) => true,
            (Self::Reject(a), Self::Reject(b)) => a.status() == b.status(),
            _ => false,
        }
    }
}

/// A built-in guard plugin.
pub trait GuardPlugin: Send + Sync {
    /// GTS identifier this plugin resolves.
    fn plugin_id(&self) -> &'static str;

    /// Validates the inbound request.
    fn guard_request(
        &self,
        headers: &axum::http::HeaderMap,
        config: &BTreeMap<String, serde_json::Value>,
    ) -> GuardVerdict {
        let _ = (headers, config);
        GuardVerdict::Allow
    }

    /// Validates the upstream response.
    fn guard_response(
        &self,
        headers: &axum::http::HeaderMap,
        config: &BTreeMap<String, serde_json::Value>,
    ) -> GuardVerdict {
        let _ = (headers, config);
        GuardVerdict::Allow
    }
}

/// A built-in transform plugin.
pub trait TransformPlugin: Send + Sync {
    /// GTS identifier this plugin resolves.
    fn plugin_id(&self) -> &'static str;

    /// Mutates the outbound request head.
    fn transform_request(&self, head: &mut RequestHead, config: &BTreeMap<String, serde_json::Value>);

    /// Mutates the response head returned to the caller.
    fn transform_response(
        &self,
        head: &mut axum::http::HeaderMap,
        config: &BTreeMap<String, serde_json::Value>,
    ) {
        let _ = (head, config);
    }
}

/// The mutable parts of an outbound request head a transform plugin may touch.
#[derive(Debug, Clone, Default)]
pub struct RequestHead {
    /// Request path, including the appended suffix.
    pub path: String,
    /// Query string, without the leading `?`.
    pub query: Option<String>,
    /// Header map to mutate in place.
    pub headers: axum::http::HeaderMap,
}

/// Registry of built-in auth plugins.
#[derive(Clone, Default)]
pub struct AuthPluginRegistry {
    plugins: Arc<BTreeMap<String, Arc<dyn AuthPlugin>>>,
}

impl AuthPluginRegistry {
    /// Registers the built-in set (`ADR-0002`, `ADR-0008`).
    #[must_use]
    pub fn with_builtins(
        secrets: Arc<dyn SecretResolver>,
        cache_ttl_secs: u64,
        cache_capacity: usize,
    ) -> Self {
        let mut plugins: BTreeMap<String, Arc<dyn AuthPlugin>> = BTreeMap::new();
        plugins.insert(
            crate::gts::AUTH_NOOP.to_owned(),
            Arc::new(super::builtins::NoopAuthPlugin) as Arc<dyn AuthPlugin>,
        );
        plugins.insert(
            crate::gts::AUTH_APIKEY.to_owned(),
            Arc::new(super::builtins::ApiKeyAuthPlugin::new()),
        );
        plugins.insert(
            crate::gts::AUTH_OAUTH2_CC.to_owned(),
            Arc::new(super::builtins::OAuth2ClientCredPlugin::form(
                secrets.clone(),
                cache_ttl_secs,
                cache_capacity,
            )),
        );
        plugins.insert(
            crate::gts::AUTH_OAUTH2_CC_BASIC.to_owned(),
            Arc::new(super::builtins::OAuth2ClientCredPlugin::basic(
                secrets,
                cache_ttl_secs,
                cache_capacity,
            )),
        );
        Self { plugins: Arc::new(plugins) }
    }

    /// Resolves a plugin by GTS identifier.
    #[must_use]
    pub fn get(&self, plugin_id: &str) -> Option<Arc<dyn AuthPlugin>> {
        self.plugins.get(plugin_id).cloned()
    }

    /// The identifiers this registry can resolve.
    #[must_use]
    pub fn ids(&self) -> Vec<String> {
        self.plugins.keys().cloned().collect()
    }
}

/// Registry of built-in guard plugins.
#[derive(Clone, Default)]
pub struct GuardPluginRegistry {
    plugins: Arc<BTreeMap<String, Arc<dyn GuardPlugin>>>,
}

impl GuardPluginRegistry {
    /// Registers the built-in set (`ADR-0009`).
    #[must_use]
    pub fn with_builtins() -> Self {
        let mut plugins: BTreeMap<String, Arc<dyn GuardPlugin>> = BTreeMap::new();
        plugins.insert(
            crate::gts::GUARD_REQUIRED_HEADERS.to_owned(),
            Arc::new(super::builtins::RequiredHeadersGuard),
        );
        Self { plugins: Arc::new(plugins) }
    }

    /// Resolves a plugin by GTS identifier.
    #[must_use]
    pub fn get(&self, plugin_id: &str) -> Option<Arc<dyn GuardPlugin>> {
        self.plugins.get(plugin_id).cloned()
    }
}

/// Registry of built-in transform plugins.
#[derive(Clone, Default)]
pub struct TransformPluginRegistry {
    plugins: Arc<BTreeMap<String, Arc<dyn TransformPlugin>>>,
}

impl TransformPluginRegistry {
    /// Registers the built-in set.
    #[must_use]
    pub fn with_builtins() -> Self {
        let mut plugins: BTreeMap<String, Arc<dyn TransformPlugin>> = BTreeMap::new();
        plugins.insert(
            crate::gts::TRANSFORM_REQUEST_ID.to_owned(),
            Arc::new(super::builtins::RequestIdTransform),
        );
        Self { plugins: Arc::new(plugins) }
    }

    /// Resolves a plugin by GTS identifier.
    #[must_use]
    pub fn get(&self, plugin_id: &str) -> Option<Arc<dyn TransformPlugin>> {
        self.plugins.get(plugin_id).cloned()
    }
}

/// Configuration object attached to a plugin reference.
///
/// `plugins.items[]` accepts either a bare identifier (a builtin GTS id or a
/// custom plugin UUID) or an object carrying `plugin_ref` plus inline
/// `config` (`ADR-0009`'s binding example).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct PluginBinding {
    /// The referenced plugin: a GTS identifier or a custom plugin UUID.
    pub plugin_ref: String,
    /// Inline configuration, when the reference carries one.
    pub config: Option<serde_json::Value>,
}

impl PluginBinding {
    /// Configuration as a sorted map, for plugin consumption.
    #[must_use]
    pub fn config_map(&self) -> BTreeMap<String, serde_json::Value> {
        match &self.config {
            Some(serde_json::Value::Object(map)) => map.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
            Some(serde_json::Value::Null) | None => BTreeMap::new(),
            Some(other) => BTreeMap::from([("value".to_owned(), other.clone())]),
        }
    }
}

/// One resolved plugin in an execution chain.
#[derive(Debug, Clone, PartialEq)]
pub enum ResolvedPlugin {
    /// A built-in plugin, resolved from its registry.
    Builtin(PluginBinding),
    /// A tenant-defined Starlark plugin, persisted in the store.
    Custom {
        /// Plugin GTS instance UUID.
        id: Uuid,
        /// Plugin kind.
        kind: crate::domain::model::PluginType,
        /// Plugin binding (name/config), for the record.
        binding: PluginBinding,
    },
}

impl ResolvedPlugin {
    /// The plugin reference as configured.
    #[must_use]
    pub fn reference(&self) -> String {
        match self {
            Self::Builtin(binding) => binding.plugin_ref.clone(),
            Self::Custom { binding, .. } => binding.plugin_ref.clone(),
        }
    }

    /// The plugin's configuration, if any.
    #[must_use]
    pub fn config_map(&self) -> BTreeMap<String, serde_json::Value> {
        match self {
            Self::Builtin(binding) | Self::Custom { binding, .. } => binding.config_map(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FixedSecrets(&'static str);

    #[async_trait::async_trait]
    impl SecretResolver for FixedSecrets {
        async fn resolve(&self, _tenant: Uuid, _reference: &str) -> anyhow::Result<Option<String>> {
            Ok(Some(self.0.to_owned()))
        }
    }

    #[test]
    fn injection_headers_ignore_unrenderable_values() {
        assert!(AuthInjection::header("x-api-key", "abc").is_some());
        assert!(AuthInjection::header("bad header name", "abc").is_none());
        assert!(AuthInjection::header("x-ok", "not\nvalid").is_none());
    }

    #[test]
    fn registries_resolve_the_builtins() {
        let secrets: Arc<dyn SecretResolver> = Arc::new(FixedSecrets("s"));
        let auth = AuthPluginRegistry::with_builtins(secrets, 300, 10_000);
        assert_eq!(auth.ids().len(), 4);
        assert!(auth.get(crate::gts::AUTH_APIKEY).is_some());
        assert!(auth.get("gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.unknown.v1").is_none());

        let guard = GuardPluginRegistry::with_builtins();
        assert!(guard.get(crate::gts::GUARD_REQUIRED_HEADERS).is_some());

        let transform = TransformPluginRegistry::with_builtins();
        assert!(transform.get(crate::gts::TRANSFORM_REQUEST_ID).is_some());
    }

    #[test]
    fn binding_config_is_normalized_to_a_sorted_map() {
        let mut binding = PluginBinding::default();
        assert!(binding.config_map().is_empty());
        binding.config = Some(serde_json::json!({"b": 2, "a": "x", "nested": {"c": 3}}));
        let map = binding.config_map();
        assert_eq!(map.len(), 3);
        assert_eq!(map["a"], serde_json::json!("x"));
        assert_eq!(map["nested"], serde_json::json!({"c": 3}));
        binding.config = Some(serde_json::json!(7));
        assert_eq!(binding.config_map()["value"], serde_json::json!(7));
    }
}
