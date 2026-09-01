//! Built-in auth- and guard-plugin registries (DESIGN "`AuthPluginRegistry`",
//! ADR 0009 "`GuardPluginRegistry`").

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use credstore_sdk::CredStoreClientV1;

use crate::domain::plugin::{
    API_KEY_AUTH_PLUGIN_ID, AuthPlugin, BASIC_AUTH_PLUGIN_ID, BEARER_AUTH_PLUGIN_ID, GuardPlugin,
    NOOP_AUTH_PLUGIN_ID, OAUTH2_CLIENT_CRED_AUTH_PLUGIN_ID,
    OAUTH2_CLIENT_CRED_BASIC_AUTH_PLUGIN_ID, REQUIRED_HEADERS_GUARD_PLUGIN_ID,
};

use super::apikey_auth::ApiKeyAuthPlugin;
use super::noop_auth::NoopAuthPlugin;
use super::oauth2_client_cred_auth::OAuth2ClientCredAuthPlugin;
use super::required_headers_guard::RequiredHeadersGuardPlugin;

/// Token-cache configuration threaded from `OagwConfig` (ADR 0008).
#[derive(Debug, Clone, Copy)]
pub struct TokenCacheConfig {
    /// Ceiling for the cached access-token TTL.
    pub ttl: Duration,
    /// Maximum number of cache entries.
    pub capacity: usize,
}

/// Resolves an upstream's `auth.type` to a built-in [`AuthPlugin`] by its GTS
/// identifier.
///
/// The catalog-only identifiers (`basic.v1`, `bearer.v1`) have no backing
/// implementation and are intentionally absent here — resolving them yields
/// `None`, which the data plane reports as `unknown auth plugin`
/// (503 `plugin.not_found.v1`).
pub struct AuthPluginRegistry {
    plugins: HashMap<&'static str, Arc<dyn AuthPlugin>>,
}

impl AuthPluginRegistry {
    /// Build the registry with the built-in plugins.
    ///
    /// When no `cred_store` client is available (provider gear absent from the
    /// binary), only `noop.v1` is registered: the credential-injecting plugins
    /// cannot resolve secrets and fail fast as unknown at request time.
    #[must_use]
    pub fn new(credstore: Option<Arc<dyn CredStoreClientV1>>, cache_cfg: TokenCacheConfig) -> Self {
        let mut plugins: HashMap<&'static str, Arc<dyn AuthPlugin>> = HashMap::new();
        plugins.insert(NOOP_AUTH_PLUGIN_ID, Arc::new(NoopAuthPlugin));

        let Some(credstore) = credstore else {
            return Self { plugins };
        };

        plugins.insert(
            API_KEY_AUTH_PLUGIN_ID,
            Arc::new(ApiKeyAuthPlugin::new(credstore.clone())),
        );
        plugins.insert(
            OAUTH2_CLIENT_CRED_AUTH_PLUGIN_ID,
            Arc::new(OAuth2ClientCredAuthPlugin::new(
                credstore.clone(),
                toolkit_auth::oauth2::ClientAuthMethod::Form,
                cache_cfg.ttl,
                cache_cfg.capacity,
            )),
        );
        plugins.insert(
            OAUTH2_CLIENT_CRED_BASIC_AUTH_PLUGIN_ID,
            Arc::new(OAuth2ClientCredAuthPlugin::new(
                credstore,
                toolkit_auth::oauth2::ClientAuthMethod::Basic,
                cache_cfg.ttl,
                cache_cfg.capacity,
            )),
        );
        Self { plugins }
    }

    /// Resolve a plugin by its exact GTS identifier.
    #[must_use]
    pub fn resolve(&self, id: &str) -> Option<Arc<dyn AuthPlugin>> {
        self.plugins.get(id).cloned()
    }

    /// The number of registered plugins (observability/tests).
    #[must_use]
    pub fn len(&self) -> usize {
        self.plugins.len()
    }

    /// Whether the registry has no plugins (infallible; always false in
    /// practice because `noop` is always registered).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.plugins.is_empty()
    }

    /// GTS identifiers that are cataloged in the types-registry but carry no
    /// implementation (DESIGN "Catalog-only identifiers").
    #[allow(dead_code)]
    pub const CATALOG_ONLY_IDS: [&'static str; 2] = [BASIC_AUTH_PLUGIN_ID, BEARER_AUTH_PLUGIN_ID];
}

/// Resolves an upstream's bound `plugins.items` entry to a built-in
/// [`GuardPlugin`] by its GTS identifier (ADR 0009).
///
/// `required_headers.v1` is the only guard identifier with a backing
/// implementation; `timeout.v1` / `cors.v1` are catalog-only (core data-plane
/// logic, not `GuardPlugin` implementations) and resolve to `None`, which the
/// data plane ignores.
pub struct GuardPluginRegistry {
    plugins: HashMap<&'static str, Arc<dyn GuardPlugin>>,
}

impl GuardPluginRegistry {
    /// Build the registry with the built-in guard plugins (ADR 0009
    /// "Registry Integration": currently only `required_headers.v1`).
    #[must_use]
    pub fn with_builtins() -> Self {
        let mut plugins: HashMap<&'static str, Arc<dyn GuardPlugin>> = HashMap::new();
        plugins.insert(
            REQUIRED_HEADERS_GUARD_PLUGIN_ID,
            Arc::new(RequiredHeadersGuardPlugin),
        );
        Self { plugins }
    }

    /// Resolve a plugin by its exact GTS identifier.
    #[must_use]
    pub fn resolve(&self, id: &str) -> Option<Arc<dyn GuardPlugin>> {
        self.plugins.get(id).cloned()
    }

    /// The number of registered plugins (observability/tests).
    #[must_use]
    pub fn len(&self) -> usize {
        self.plugins.len()
    }

    /// Whether the registry has no plugins (infallible; always false with
    /// builtins).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.plugins.is_empty()
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use crate::domain::plugin::{
        API_KEY_AUTH_PLUGIN_ID, NOOP_AUTH_PLUGIN_ID, OAUTH2_CLIENT_CRED_AUTH_PLUGIN_ID,
        OAUTH2_CLIENT_CRED_BASIC_AUTH_PLUGIN_ID,
    };

    fn cache_cfg() -> TokenCacheConfig {
        TokenCacheConfig {
            ttl: Duration::from_mins(5),
            capacity: 10,
        }
    }

    #[test]
    fn with_credstore_registers_all_builtins() {
        let registry = AuthPluginRegistry::new(
            Some(Arc::new(
                credstore_sdk::test_util::MockCredStoreClient::empty(),
            )),
            cache_cfg(),
        );
        for id in [
            NOOP_AUTH_PLUGIN_ID,
            API_KEY_AUTH_PLUGIN_ID,
            OAUTH2_CLIENT_CRED_AUTH_PLUGIN_ID,
            OAUTH2_CLIENT_CRED_BASIC_AUTH_PLUGIN_ID,
        ] {
            assert!(registry.resolve(id).is_some(), "{id} must be registered");
            assert_eq!(registry.resolve(id).unwrap().id(), id);
        }
    }

    #[test]
    fn without_credstore_only_noop_is_registered() {
        let registry = AuthPluginRegistry::new(None, cache_cfg());
        assert!(registry.resolve(NOOP_AUTH_PLUGIN_ID).is_some());
        assert!(registry.resolve(API_KEY_AUTH_PLUGIN_ID).is_none());
    }

    #[test]
    fn catalog_only_ids_are_not_resolvable() {
        let registry = AuthPluginRegistry::new(
            Some(Arc::new(
                credstore_sdk::test_util::MockCredStoreClient::empty(),
            )),
            cache_cfg(),
        );
        for id in AuthPluginRegistry::CATALOG_ONLY_IDS {
            assert!(registry.resolve(id).is_none(), "{id} must not resolve");
        }
    }

    #[test]
    fn guard_registry_resolves_required_headers_only() {
        let registry = GuardPluginRegistry::with_builtins();
        assert_eq!(registry.len(), 1);
        let plugin = registry.resolve(crate::domain::plugin::REQUIRED_HEADERS_GUARD_PLUGIN_ID);
        assert!(plugin.is_some(), "required_headers.v1 must be registered");
        assert_eq!(
            plugin.unwrap().id(),
            crate::domain::plugin::REQUIRED_HEADERS_GUARD_PLUGIN_ID
        );
        // Catalog-only/custom/unknown ids resolve to None and are ignored.
        assert!(
            registry
                .resolve("gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.timeout.v1")
                .is_none()
        );
        assert!(registry.resolve("some-custom-plugin-uuid").is_none());
    }
}
