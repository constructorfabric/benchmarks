// Created: 2026-09-01 by Constructor Tech
//! The plugin registries.
//!
//! `docs/DESIGN.md` §3.1 "Plugin Identification Model": named plugins are
//! resolved from an in-process registry, custom plugins from the store.
//! `basic`/`bearer` (auth), `timeout`/`cors` (guard) and
//! `logging`/`metrics` (transform) are catalog-only identifiers with no
//! backing plugin, so a lookup for them returns `PluginNotFound`.

use std::collections::BTreeMap;
use std::sync::Arc;

use crate::domain::errors::OagwError;
use crate::infra::credstore::SecretResolver;
use crate::infra::plugin::apikey::ApiKeyAuthPlugin;
use crate::infra::plugin::noop::NoopAuthPlugin;
use crate::infra::plugin::oauth2::{OAuth2ClientCredAuthPlugin, TokenCacheConfig};
use crate::infra::plugin::request_id::RequestIdTransformPlugin;
use crate::infra::plugin::required_headers::RequiredHeadersGuardPlugin;
use crate::infra::plugin::traits::{AuthPlugin, GuardPlugin, TransformPlugin};

/// The auth plugin registry.
#[derive(Clone)]
pub struct AuthPluginRegistry {
    plugins: BTreeMap<String, Arc<dyn AuthPlugin>>,
}

impl std::fmt::Debug for AuthPluginRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuthPluginRegistry")
            .field("plugins", &self.plugins.keys().collect::<Vec<_>>())
            .finish()
    }
}

impl AuthPluginRegistry {
    /// Register the built-in auth plugins.
    #[must_use]
    pub fn with_builtins(
        resolver: SecretResolver,
        http_config: Option<toolkit_http::HttpClientConfig>,
        cache: TokenCacheConfig,
    ) -> Self {
        let mut plugins: BTreeMap<String, Arc<dyn AuthPlugin>> = BTreeMap::new();
        plugins.insert(NoopAuthPlugin.id().to_owned(), Arc::new(NoopAuthPlugin));
        let apikey = ApiKeyAuthPlugin::new(resolver.clone());
        plugins.insert(apikey.id().to_owned(), Arc::new(apikey));
        let ttl = cache.ttl;
        let capacity = cache.capacity;
        let form = OAuth2ClientCredAuthPlugin::new(
            resolver.clone(),
            toolkit_auth::oauth2::ClientAuthMethod::Form,
            ttl,
            capacity,
        );
        let basic = OAuth2ClientCredAuthPlugin::new(
            resolver,
            toolkit_auth::oauth2::ClientAuthMethod::Basic,
            ttl,
            capacity,
        );
        let (form, basic) = match http_config {
            Some(cfg) => (
                form.with_http_config(cfg.clone()),
                basic.with_http_config(cfg),
            ),
            None => (form, basic),
        };
        plugins.insert(form.id().to_owned(), Arc::new(form));
        plugins.insert(basic.id().to_owned(), Arc::new(basic));
        Self { plugins }
    }

    /// Resolve a plugin by GTS identifier.
    ///
    /// # Errors
    /// Returns `PluginNotFound` for an unregistered identifier.
    pub fn resolve(&self, id: &str) -> Result<Arc<dyn AuthPlugin>, OagwError> {
        self.plugins
            .get(id)
            .cloned()
            .ok_or_else(|| OagwError::plugin_not_found(&format!("unknown auth plugin '{id}'")))
    }

    /// The registered identifiers.
    #[must_use]
    pub fn ids(&self) -> Vec<&str> {
        self.plugins.keys().map(String::as_str).collect()
    }
}

/// The guard plugin registry.
#[derive(Clone, Default)]
pub struct GuardPluginRegistry {
    plugins: BTreeMap<String, Arc<dyn GuardPlugin>>,
}

impl std::fmt::Debug for GuardPluginRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GuardPluginRegistry")
            .field("plugins", &self.plugins.keys().collect::<Vec<_>>())
            .finish()
    }
}

impl GuardPluginRegistry {
    /// Register the built-in guard plugins. `required_headers` is the only
    /// entry: timeout and CORS are core Data Plane functionality.
    #[must_use]
    pub fn with_builtins() -> Self {
        let mut plugins: BTreeMap<String, Arc<dyn GuardPlugin>> = BTreeMap::new();
        plugins.insert(
            RequiredHeadersGuardPlugin.id().to_owned(),
            Arc::new(RequiredHeadersGuardPlugin),
        );
        Self { plugins }
    }

    /// Resolve a plugin by GTS identifier.
    ///
    /// # Errors
    /// Returns `PluginNotFound` for an unregistered identifier.
    pub fn resolve(&self, id: &str) -> Result<Arc<dyn GuardPlugin>, OagwError> {
        self.plugins
            .get(id)
            .cloned()
            .ok_or_else(|| OagwError::plugin_not_found(&format!("unknown guard plugin '{id}'")))
    }

    /// The registered identifiers.
    #[must_use]
    pub fn ids(&self) -> Vec<&str> {
        self.plugins.keys().map(String::as_str).collect()
    }
}

/// The transform plugin registry.
#[derive(Clone, Default)]
pub struct TransformPluginRegistry {
    plugins: BTreeMap<String, Arc<dyn TransformPlugin>>,
}

impl std::fmt::Debug for TransformPluginRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TransformPluginRegistry")
            .field("plugins", &self.plugins.keys().collect::<Vec<_>>())
            .finish()
    }
}

impl TransformPluginRegistry {
    /// Register the built-in transform plugins. `request_id` is the only
    /// entry: logging and metrics are Data Plane instrumentation.
    #[must_use]
    pub fn with_builtins() -> Self {
        let mut plugins: BTreeMap<String, Arc<dyn TransformPlugin>> = BTreeMap::new();
        plugins.insert(
            RequestIdTransformPlugin.id().to_owned(),
            Arc::new(RequestIdTransformPlugin),
        );
        Self { plugins }
    }

    /// Resolve a plugin by GTS identifier.
    ///
    /// # Errors
    /// Returns `PluginNotFound` for an unregistered identifier.
    pub fn resolve(&self, id: &str) -> Result<Arc<dyn TransformPlugin>, OagwError> {
        self.plugins
            .get(id)
            .cloned()
            .ok_or_else(|| OagwError::plugin_not_found(&format!("unknown transform plugin '{id}'")))
    }

    /// The registered identifiers.
    #[must_use]
    pub fn ids(&self) -> Vec<&str> {
        self.plugins.keys().map(String::as_str).collect()
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use crate::domain::model::builtin_plugins as bp;

    fn auth_registry() -> AuthPluginRegistry {
        AuthPluginRegistry::with_builtins(
            SecretResolver::unlinked(),
            None,
            TokenCacheConfig::default(),
        )
    }

    #[test]
    fn the_built_in_auth_plugins_are_registered() {
        let registry = auth_registry();
        let mut ids = registry.ids();
        ids.sort_unstable();
        assert_eq!(
            ids,
            vec![
                bp::AUTH_APIKEY,
                bp::AUTH_NOOP,
                bp::AUTH_OAUTH2_FORM,
                bp::AUTH_OAUTH2_BASIC,
            ]
        );
    }

    #[test]
    fn catalog_only_auth_plugins_do_not_resolve() {
        for id in [bp::AUTH_BASIC, bp::AUTH_BEARER] {
            let err = match auth_registry().resolve(id) {
                Ok(_) => panic!("{id} should not resolve"),
                Err(err) => err,
            };
            assert_eq!(err.status_value(), 503, "{id}");
            assert!(err.detail().contains("unknown auth plugin"), "{err}");
        }
    }

    #[test]
    fn required_headers_is_the_only_guard() {
        let registry = GuardPluginRegistry::with_builtins();
        assert_eq!(registry.ids(), vec![bp::GUARD_REQUIRED_HEADERS]);
        assert!(registry.resolve(bp::GUARD_TIMEOUT).is_err());
        assert!(registry.resolve(bp::GUARD_CORS).is_err());
    }

    #[test]
    fn request_id_is_the_only_transform() {
        let registry = TransformPluginRegistry::with_builtins();
        assert_eq!(registry.ids(), vec![bp::TRANSFORM_REQUEST_ID]);
        for id in [bp::TRANSFORM_LOGGING, bp::TRANSFORM_METRICS] {
            let err = match registry.resolve(id) {
                Ok(_) => panic!("{id} should not resolve"),
                Err(err) => err,
            };
            assert_eq!(err.status_value(), 503, "{id}");
        }
    }
}
