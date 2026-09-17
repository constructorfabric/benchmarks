//! Plugin registries and the built-in plugin set.
//!
//! Registries resolve a GTS identifier to an in-process plugin instance.
//! Identifiers marked catalog-only by the design documents (`basic`, `bearer`,
//! `timeout`, `cors`, `logging`, `metrics`) are intentionally **not**
//! registered; binding them yields `PluginNotFound`.

use std::collections::HashMap;
use std::sync::Arc;

use crate::domain::gts_helpers::{CATALOG_ONLY_PLUGIN_IDS, NOOP_AUTH_PLUGIN_ID};
use crate::domain::plugin::{AuthPlugin, GuardPlugin, SecretResolver, TransformPlugin};

/// Registry of auth plugins keyed by GTS identifier.
#[derive(Default)]
pub struct AuthPluginRegistry {
    plugins: HashMap<String, Arc<dyn AuthPlugin>>,
}

/// Registry of guard plugins keyed by GTS identifier.
#[derive(Default)]
pub struct GuardPluginRegistry {
    plugins: HashMap<String, Arc<dyn GuardPlugin>>,
}

/// Registry of transform plugins keyed by GTS identifier.
#[derive(Default)]
pub struct TransformPluginRegistry {
    plugins: HashMap<String, Arc<dyn TransformPlugin>>,
}

impl AuthPluginRegistry {
    /// Builds the built-in auth set.
    ///
    /// # Panics
    ///
    /// Never: registration is backed by in-memory maps.
    #[must_use]
    pub fn with_builtins(resolver: Arc<dyn SecretResolver>) -> Self {
        let mut registry = Self::default();
        registry.register(
            crate::domain::gts_helpers::APIKEY_AUTH_PLUGIN_ID,
            Arc::new(super::apikey_auth::ApiKeyAuthPlugin),
        );
        registry.register(
            NOOP_AUTH_PLUGIN_ID,
            Arc::new(super::noop_auth::NoopAuthPlugin),
        );
        registry.register(
            crate::domain::gts_helpers::OAUTH2_CLIENT_CRED_AUTH_PLUGIN_ID,
            Arc::new(
                super::oauth2_client_cred_auth::OAuth2ClientCredAuthPlugin::form(resolver.clone()),
            ),
        );
        registry.register(
            crate::domain::gts_helpers::OAUTH2_CLIENT_CRED_BASIC_AUTH_PLUGIN_ID,
            Arc::new(super::oauth2_client_cred_auth::OAuth2ClientCredAuthPlugin::basic(resolver)),
        );
        registry
    }

    /// Registers a plugin, replacing any previous binding for the same id.
    pub fn register(&mut self, id: &str, plugin: Arc<dyn AuthPlugin>) {
        self.plugins.insert(id.to_owned(), plugin);
    }

    /// Resolves a plugin, treating catalog-only identifiers as absent.
    #[must_use]
    pub fn get(&self, id: &str) -> Option<Arc<dyn AuthPlugin>> {
        if CATALOG_ONLY_PLUGIN_IDS.contains(&id) {
            return None;
        }
        self.plugins.get(id).cloned()
    }

    /// `true` when the identifier is known to be catalog-only.
    #[must_use]
    pub fn is_catalog_only(id: &str) -> bool {
        CATALOG_ONLY_PLUGIN_IDS.contains(&id)
    }
}

impl GuardPluginRegistry {
    /// Builds the built-in guard registry.
    #[must_use]
    pub fn with_builtins() -> Self {
        let mut registry = Self::default();
        registry.register(
            crate::domain::gts_helpers::REQUIRED_HEADERS_GUARD_PLUGIN_ID,
            Arc::new(super::required_headers_guard::RequiredHeadersGuardPlugin),
        );
        registry
    }

    /// Registers a guard plugin.
    pub fn register(&mut self, id: &str, plugin: Arc<dyn GuardPlugin>) {
        self.plugins.insert(id.to_owned(), plugin);
    }

    /// Resolves a guard plugin.
    #[must_use]
    pub fn get(&self, id: &str) -> Option<Arc<dyn GuardPlugin>> {
        if CATALOG_ONLY_PLUGIN_IDS.contains(&id) {
            return None;
        }
        self.plugins.get(id).cloned()
    }
}

impl TransformPluginRegistry {
    /// Builds the built-in transform registry.
    #[must_use]
    pub fn with_builtins() -> Self {
        let mut registry = Self::default();
        registry.register(
            crate::domain::gts_helpers::REQUEST_ID_TRANSFORM_PLUGIN_ID,
            Arc::new(super::request_id_transform::RequestIdTransformPlugin),
        );
        registry
    }

    /// Registers a transform plugin.
    pub fn register(&mut self, id: &str, plugin: Arc<dyn TransformPlugin>) {
        self.plugins.insert(id.to_owned(), plugin);
    }

    /// Resolves a transform plugin.
    #[must_use]
    pub fn get(&self, id: &str) -> Option<Arc<dyn TransformPlugin>> {
        if CATALOG_ONLY_PLUGIN_IDS.contains(&id) {
            return None;
        }
        self.plugins.get(id).cloned()
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    fn noop_resolver() -> Arc<dyn SecretResolver> {
        struct Nop;

        #[async_trait::async_trait]
        impl SecretResolver for Nop {
            async fn resolve(
                &self,
                _ctx: &toolkit_security::SecurityContext,
                _reference: &str,
            ) -> Result<
                Option<crate::domain::plugin::ResolvedSecret>,
                crate::domain::plugin::PluginError,
            > {
                Ok(None)
            }
        }
        Arc::new(Nop)
    }

    #[test]
    fn builtins_are_registered_and_catalog_ids_are_not() {
        let auth = AuthPluginRegistry::with_builtins(noop_resolver());
        assert!(
            auth.get(crate::domain::gts_helpers::APIKEY_AUTH_PLUGIN_ID)
                .is_some()
        );
        assert!(
            auth.get(crate::domain::gts_helpers::NOOP_AUTH_PLUGIN_ID)
                .is_some()
        );
        assert!(
            auth.get(crate::domain::gts_helpers::OAUTH2_CLIENT_CRED_AUTH_PLUGIN_ID)
                .is_some()
        );
        assert!(
            auth.get(crate::domain::gts_helpers::OAUTH2_CLIENT_CRED_BASIC_AUTH_PLUGIN_ID)
                .is_some()
        );
        assert!(
            auth.get("gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.basic.v1")
                .is_none()
        );
        assert!(auth.get("unknown").is_none());
        assert!(AuthPluginRegistry::is_catalog_only(
            "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.timeout.v1"
        ));

        let guard = GuardPluginRegistry::with_builtins();
        assert!(
            guard
                .get(crate::domain::gts_helpers::REQUIRED_HEADERS_GUARD_PLUGIN_ID)
                .is_some()
        );
        assert!(
            guard
                .get("gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.cors.v1")
                .is_none()
        );

        let transform = TransformPluginRegistry::with_builtins();
        assert!(
            transform
                .get(crate::domain::gts_helpers::REQUEST_ID_TRANSFORM_PLUGIN_ID)
                .is_some()
        );
        assert!(
            transform
                .get("gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.metrics.v1")
                .is_none()
        );
    }
}
