//! In-process plugin registries.
//!
//! Named plugins live here and are never persisted. UUID-backed custom plugins
//! are stored in the plugin repository instead; the instance part of the GTS
//! identifier decides which side resolves it
//! (`docs/DESIGN.md` §"Resolution Algorithm").

use std::collections::HashMap;
use std::sync::Arc;

use credstore_sdk::CredStoreClientV1;
use toolkit_auth::oauth2::ClientAuthMethod;

use crate::domain::gts;
use crate::domain::plugin::{AuthPlugin, GuardPlugin, TransformPlugin};

use super::apikey_auth::ApiKeyAuthPlugin;
use super::noop_auth::NoopAuthPlugin;
use super::oauth2_client_cred_auth::{OAuth2ClientCredAuthPlugin, TokenCacheConfig};
use super::request_id_transform::RequestIdTransformPlugin;
use super::required_headers_guard::RequiredHeadersGuardPlugin;

/// Registry of auth plugins, keyed by full GTS identifier.
#[derive(Default)]
pub struct AuthPluginRegistry {
    plugins: HashMap<String, Arc<dyn AuthPlugin>>,
}

impl AuthPluginRegistry {
    /// Register every built-in auth plugin.
    ///
    /// `basic` and `bearer` are deliberately absent: they are catalog
    /// identifiers with no backing implementation, so binding one fails with
    /// `unknown auth plugin`.
    #[must_use]
    pub fn with_builtins(
        credstore: Arc<dyn CredStoreClientV1>,
        token_cache: TokenCacheConfig,
    ) -> Self {
        let mut plugins: HashMap<String, Arc<dyn AuthPlugin>> = HashMap::new();
        plugins.insert(
            gts::NOOP_AUTH_PLUGIN_ID.to_owned(),
            Arc::new(NoopAuthPlugin),
        );
        plugins.insert(
            gts::APIKEY_AUTH_PLUGIN_ID.to_owned(),
            Arc::new(ApiKeyAuthPlugin::new(Arc::clone(&credstore))),
        );
        plugins.insert(
            gts::OAUTH2_CLIENT_CRED_AUTH_PLUGIN_ID.to_owned(),
            Arc::new(OAuth2ClientCredAuthPlugin::new(
                Arc::clone(&credstore),
                ClientAuthMethod::Form,
                token_cache,
            )),
        );
        plugins.insert(
            gts::OAUTH2_CLIENT_CRED_BASIC_AUTH_PLUGIN_ID.to_owned(),
            Arc::new(OAuth2ClientCredAuthPlugin::new(
                credstore,
                ClientAuthMethod::Basic,
                token_cache,
            )),
        );
        Self { plugins }
    }

    /// Add or replace a plugin, e.g. one contributed by another gear.
    pub fn register(&mut self, plugin: Arc<dyn AuthPlugin>) {
        self.plugins.insert(plugin.plugin_type().to_owned(), plugin);
    }

    #[must_use]
    pub fn get(&self, gts_id: &str) -> Option<Arc<dyn AuthPlugin>> {
        self.plugins.get(gts_id).map(Arc::clone)
    }

    #[must_use]
    pub fn ids(&self) -> Vec<&str> {
        self.plugins.keys().map(String::as_str).collect()
    }
}

/// Registry of guard plugins, keyed by full GTS identifier.
#[derive(Default)]
pub struct GuardPluginRegistry {
    plugins: HashMap<String, Arc<dyn GuardPlugin>>,
}

impl GuardPluginRegistry {
    /// `required_headers` is the only built-in guard: timeout and CORS are
    /// core Data Plane logic, not `GuardPlugin` implementations.
    #[must_use]
    pub fn with_builtins() -> Self {
        let mut plugins: HashMap<String, Arc<dyn GuardPlugin>> = HashMap::new();
        plugins.insert(
            gts::REQUIRED_HEADERS_GUARD_PLUGIN_ID.to_owned(),
            Arc::new(RequiredHeadersGuardPlugin),
        );
        Self { plugins }
    }

    pub fn register(&mut self, plugin: Arc<dyn GuardPlugin>) {
        self.plugins.insert(plugin.plugin_type().to_owned(), plugin);
    }

    #[must_use]
    pub fn get(&self, gts_id: &str) -> Option<Arc<dyn GuardPlugin>> {
        self.plugins.get(gts_id).map(Arc::clone)
    }
}

/// Registry of transform plugins, keyed by full GTS identifier.
#[derive(Default)]
pub struct TransformPluginRegistry {
    plugins: HashMap<String, Arc<dyn TransformPlugin>>,
}

impl TransformPluginRegistry {
    /// `request_id` is the only built-in transform: logging and metrics are
    /// core Data Plane instrumentation.
    #[must_use]
    pub fn with_builtins() -> Self {
        let mut plugins: HashMap<String, Arc<dyn TransformPlugin>> = HashMap::new();
        plugins.insert(
            gts::REQUEST_ID_TRANSFORM_PLUGIN_ID.to_owned(),
            Arc::new(RequestIdTransformPlugin),
        );
        Self { plugins }
    }

    pub fn register(&mut self, plugin: Arc<dyn TransformPlugin>) {
        self.plugins.insert(plugin.plugin_type().to_owned(), plugin);
    }

    #[must_use]
    pub fn get(&self, gts_id: &str) -> Option<Arc<dyn TransformPlugin>> {
        self.plugins.get(gts_id).map(Arc::clone)
    }
}

/// The three registries, wired once during gear init.
pub struct PluginRegistries {
    pub auth: AuthPluginRegistry,
    pub guard: GuardPluginRegistry,
    pub transform: TransformPluginRegistry,
}

impl PluginRegistries {
    #[must_use]
    pub fn with_builtins(
        credstore: Arc<dyn CredStoreClientV1>,
        token_cache: TokenCacheConfig,
    ) -> Self {
        Self {
            auth: AuthPluginRegistry::with_builtins(credstore, token_cache),
            guard: GuardPluginRegistry::with_builtins(),
            transform: TransformPluginRegistry::with_builtins(),
        }
    }
}

#[cfg(test)]
#[path = "registry_tests.rs"]
mod tests;
