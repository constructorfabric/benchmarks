//! The three in-process plugin registries (ADR-0002 "Plugin Loading").
//!
//! A registry maps a **full GTS identifier** to the implementation that backs
//! it. The identifier is the whole `PluginRef` string —
//! `gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1` — because the base
//! type alone does not name a plugin, and the instance segment of a *custom*
//! plugin is a UUID that is resolved through the plugin store instead (DESIGN
//! §3.1 "Resolution Algorithm", implemented by the engine).
//!
//! Built-ins are registered by [`PluginRegistries::with_builtins`]; an external
//! gear registers its own implementations with `register_*`, and a later
//! registration under an already-known identifier replaces the earlier one —
//! the same last-writer-wins rule ADR-0002's sketch uses.

use std::collections::HashMap;
use std::sync::Arc;

use credstore_sdk::api::CredStoreClientV1;

use super::traits::{AuthPlugin, GuardPlugin, TransformPlugin};

// The two OAuth2 identifiers are declared in `domain::types`, next to
// `AUTH_PLUGIN_TYPE_ID` and with the `auth.config` keys write-time validation
// enforces: they are read there, and a declaration site in `infra` would be the
// crate's only `domain -> infra` edge — a module cycle. Re-exported here so the
// registry stays the one place a plugin id is *resolved* from.
pub use crate::domain::types::{
    OAUTH2_CLIENT_CRED_AUTH_PLUGIN_REF, OAUTH2_CLIENT_CRED_BASIC_AUTH_PLUGIN_REF,
};

/// `gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.noop.v1` (DESIGN §3.1).
pub const NOOP_AUTH_PLUGIN_REF: &str = "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.noop.v1";
/// `gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1` (DESIGN §3.1).
pub const API_KEY_AUTH_PLUGIN_REF: &str = "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1";
/// `gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1`
/// (DESIGN §3.1, ADR-0009).
pub const REQUIRED_HEADERS_GUARD_PLUGIN_REF: &str =
    "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1";
/// `gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.request_id.v1`
/// (DESIGN §3.1).
pub const REQUEST_ID_TRANSFORM_PLUGIN_REF: &str =
    "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.request_id.v1";

/// The auth plugins of the process (ADR-0002 "Plugin Loading").
#[derive(Default)]
pub struct AuthPluginRegistry {
    plugins: HashMap<String, Arc<dyn AuthPlugin>>,
}

/// The guard plugins of the process (ADR-0002 "Plugin Loading").
#[derive(Default)]
pub struct GuardPluginRegistry {
    plugins: HashMap<String, Arc<dyn GuardPlugin>>,
}

/// The transform plugins of the process (ADR-0002 "Plugin Loading").
#[derive(Default)]
pub struct TransformPluginRegistry {
    plugins: HashMap<String, Arc<dyn TransformPlugin>>,
}

/// The identifiers a registry holds; the implementations are not printable.
impl std::fmt::Debug for AuthPluginRegistry {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AuthPluginRegistry")
            .field("plugins", &self.plugins.keys().collect::<Vec<_>>())
            .finish()
    }
}

impl std::fmt::Debug for GuardPluginRegistry {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("GuardPluginRegistry")
            .field("plugins", &self.plugins.keys().collect::<Vec<_>>())
            .finish()
    }
}

impl std::fmt::Debug for TransformPluginRegistry {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("TransformPluginRegistry")
            .field("plugins", &self.plugins.keys().collect::<Vec<_>>())
            .finish()
    }
}

impl AuthPluginRegistry {
    /// An empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Register `plugin` under its own identifier, replacing any earlier one.
    pub fn register(&mut self, plugin: Arc<dyn AuthPlugin>) {
        self.plugins.insert(plugin.plugin_ref().to_owned(), plugin);
    }

    /// The plugin a reference names, when one is registered.
    #[must_use]
    pub fn resolve(&self, reference: &str) -> Option<Arc<dyn AuthPlugin>> {
        self.plugins.get(reference).cloned()
    }

    /// The number of registered plugins, for tests and operators.
    #[must_use]
    pub fn len(&self) -> usize {
        self.plugins.len()
    }

    /// `true` when no plugin is registered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.plugins.is_empty()
    }

    /// The registered identifiers, for tests.
    #[must_use]
    pub fn keys(&self) -> Vec<String> {
        self.plugins.keys().cloned().collect()
    }
}

impl GuardPluginRegistry {
    /// An empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Register `plugin` under its own identifier, replacing any earlier one.
    pub fn register(&mut self, plugin: Arc<dyn GuardPlugin>) {
        self.plugins.insert(plugin.plugin_ref().to_owned(), plugin);
    }

    /// The plugin a reference names, when one is registered.
    #[must_use]
    pub fn resolve(&self, reference: &str) -> Option<Arc<dyn GuardPlugin>> {
        self.plugins.get(reference).cloned()
    }

    /// The number of registered plugins, for tests and operators.
    #[must_use]
    pub fn len(&self) -> usize {
        self.plugins.len()
    }

    /// `true` when no plugin is registered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.plugins.is_empty()
    }

    /// The registered identifiers, for tests.
    #[must_use]
    pub fn keys(&self) -> Vec<String> {
        self.plugins.keys().cloned().collect()
    }
}

impl TransformPluginRegistry {
    /// An empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Register `plugin` under its own identifier, replacing any earlier one.
    pub fn register(&mut self, plugin: Arc<dyn TransformPlugin>) {
        self.plugins.insert(plugin.plugin_ref().to_owned(), plugin);
    }

    /// The plugin a reference names, when one is registered.
    #[must_use]
    pub fn resolve(&self, reference: &str) -> Option<Arc<dyn TransformPlugin>> {
        self.plugins.get(reference).cloned()
    }

    /// The number of registered plugins, for tests and operators.
    #[must_use]
    pub fn len(&self) -> usize {
        self.plugins.len()
    }

    /// `true` when no plugin is registered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.plugins.is_empty()
    }

    /// The registered identifiers, for tests.
    #[must_use]
    pub fn keys(&self) -> Vec<String> {
        self.plugins.keys().cloned().collect()
    }
}

/// The three registries together, as the engine reads them (ADR-0002
/// `ControlPlane`'s three maps).
#[derive(Debug, Default)]
pub struct PluginRegistries {
    /// Auth plugins.
    pub auth: AuthPluginRegistry,
    /// Guard plugins.
    pub guard: GuardPluginRegistry,
    /// Transform plugins.
    pub transform: TransformPluginRegistry,
}

impl PluginRegistries {
    /// The registries holding the crate's built-in plugins (ADR-0002 "Built-in
    /// Plugins", DESIGN §3.1).
    #[must_use]
    pub fn with_builtins() -> Self {
        Self::with_builtins_and(None)
    }

    /// The built-in plugins, with `cred://` references resolved through
    /// `credstore` (DESIGN §2.1 "Credential Isolation") and the OAuth2 token
    /// cache at its defaults (ADR-0008 "Gear-Level Configuration").
    ///
    /// The gear installs the client it gets from the host; `None` leaves every
    /// reference unresolved, which fails the request that needs one instead of
    /// forwarding it unauthenticated.
    #[must_use]
    pub fn with_builtins_and(credstore: Option<Arc<dyn CredStoreClientV1>>) -> Self {
        Self::with_builtins_and_config(
            credstore,
            super::oauth2_client_cred_auth::TokenCacheConfig::default(),
        )
    }

    /// The built-in plugins, with the credential store and the OAuth2 token
    /// cache configuration the operator asked for (ADR-0008 "Gear-Level
    /// Configuration").
    ///
    /// Both OAuth2 variants are registered here: they differ in nothing but how
    /// the client credentials travel to the token endpoint, and they share the
    /// same cache settings.
    #[must_use]
    pub fn with_builtins_and_config(
        credstore: Option<Arc<dyn CredStoreClientV1>>,
        token_cache: super::oauth2_client_cred_auth::TokenCacheConfig,
    ) -> Self {
        let mut registries = Self::default();
        registries
            .auth
            .register(Arc::new(super::noop_auth::NoopAuthPlugin));
        registries.auth.register(Arc::new(
            super::api_key_auth::ApiKeyAuthPlugin::with_client(credstore.clone()),
        ));
        registries.auth.register(Arc::new(
            super::oauth2_client_cred_auth::OAuth2ClientCredAuthPlugin::new(
                credstore.clone(),
                toolkit_auth::oauth2::ClientAuthMethod::Form,
                token_cache.ttl,
                token_cache.capacity,
            ),
        ));
        registries.auth.register(Arc::new(
            super::oauth2_client_cred_auth::OAuth2ClientCredAuthPlugin::new(
                credstore,
                toolkit_auth::oauth2::ClientAuthMethod::Basic,
                token_cache.ttl,
                token_cache.capacity,
            ),
        ));
        registries.guard.register(Arc::new(
            super::required_headers_guard::RequiredHeadersGuardPlugin,
        ));
        registries.transform.register(Arc::new(
            super::request_id_transform::RequestIdTransformPlugin,
        ));
        registries
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::types::{
        AUTH_PLUGIN_TYPE_ID, GUARD_PLUGIN_TYPE_ID, TRANSFORM_PLUGIN_TYPE_ID,
    };
    use crate::error::OagwError;
    use http::HeaderMap;

    struct Stub;

    #[async_trait::async_trait]
    impl AuthPlugin for Stub {
        fn plugin_ref(&self) -> &str {
            API_KEY_AUTH_PLUGIN_REF
        }

        async fn authenticate(
            &self,
            _context: &super::super::traits::PluginContext<'_>,
            _headers: &mut HeaderMap,
        ) -> Result<(), OagwError> {
            Ok(())
        }
    }

    #[test]
    fn the_built_in_identifiers_use_the_declared_base_types() {
        // One declaration site for the base types: the built-in identifiers are
        // full identifiers, and this is what keeps them in step with it.
        assert!(NOOP_AUTH_PLUGIN_REF.starts_with(AUTH_PLUGIN_TYPE_ID));
        assert!(API_KEY_AUTH_PLUGIN_REF.starts_with(AUTH_PLUGIN_TYPE_ID));
        assert!(REQUIRED_HEADERS_GUARD_PLUGIN_REF.starts_with(GUARD_PLUGIN_TYPE_ID));
        assert!(REQUEST_ID_TRANSFORM_PLUGIN_REF.starts_with(TRANSFORM_PLUGIN_TYPE_ID));
        assert_eq!(
            NOOP_AUTH_PLUGIN_REF.rsplit('~').next().ok_or("instance"),
            Ok("cf.core.oagw.noop.v1")
        );
    }

    #[test]
    fn with_builtins_registers_one_plugin_of_each_kind() {
        let registries = PluginRegistries::with_builtins();

        assert_eq!(registries.auth.len(), 4);
        assert_eq!(registries.guard.len(), 1);
        assert_eq!(registries.transform.len(), 1);
        assert_eq!(
            registries.guard.keys(),
            vec![REQUIRED_HEADERS_GUARD_PLUGIN_REF.to_owned()]
        );
        assert!(
            registries
                .auth
                .keys()
                .contains(&OAUTH2_CLIENT_CRED_AUTH_PLUGIN_REF.to_owned()),
            "the OAuth2 Form variant is a built-in"
        );
        assert!(
            registries
                .auth
                .keys()
                .contains(&OAUTH2_CLIENT_CRED_BASIC_AUTH_PLUGIN_REF.to_owned()),
            "the OAuth2 Basic variant is a built-in"
        );
    }

    #[test]
    fn both_oauth2_variants_are_registered_on_the_config_carrying_path_too() {
        let registries = PluginRegistries::with_builtins_and_config(
            None,
            super::super::oauth2_client_cred_auth::TokenCacheConfig::default(),
        );

        assert_eq!(registries.auth.len(), 4);
        assert!(
            registries
                .auth
                .resolve(OAUTH2_CLIENT_CRED_AUTH_PLUGIN_REF)
                .is_some()
        );
        assert!(
            registries
                .auth
                .resolve(OAUTH2_CLIENT_CRED_BASIC_AUTH_PLUGIN_REF)
                .is_some()
        );
    }

    #[test]
    fn the_declared_oauth2_identifiers_use_the_auth_plugin_base_type() {
        assert!(OAUTH2_CLIENT_CRED_AUTH_PLUGIN_REF.starts_with(AUTH_PLUGIN_TYPE_ID));
        assert!(OAUTH2_CLIENT_CRED_BASIC_AUTH_PLUGIN_REF.starts_with(AUTH_PLUGIN_TYPE_ID));
        assert_eq!(
            OAUTH2_CLIENT_CRED_AUTH_PLUGIN_REF
                .rsplit('~')
                .next()
                .ok_or("instance"),
            Ok("cf.core.oagw.oauth2_client_cred.v1")
        );
        assert_eq!(
            OAUTH2_CLIENT_CRED_BASIC_AUTH_PLUGIN_REF
                .rsplit('~')
                .next()
                .ok_or("instance"),
            Ok("cf.core.oagw.oauth2_client_cred_basic.v1")
        );
    }

    #[test]
    fn a_registry_resolves_by_full_identifier_only() {
        let mut registry = AuthPluginRegistry::new();
        registry.register(Arc::new(Stub));

        assert!(registry.resolve(API_KEY_AUTH_PLUGIN_REF).is_some());
        assert!(
            registry
                .resolve("gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.noop.v1")
                .is_none(),
            "a different instance of the same base type is a different plugin"
        );
        assert!(
            registry
                .resolve("gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.apikey.v1")
                .is_none(),
            "the base type alone does not resolve a plugin"
        );
        assert!(registry.resolve("").is_none());
    }

    #[test]
    fn a_registration_replaces_an_earlier_one_under_the_same_identifier() {
        let shared: Arc<dyn AuthPlugin> = Arc::new(Stub);
        let mut registry = AuthPluginRegistry::new();
        registry.register(Arc::clone(&shared));
        let first = registry.resolve(API_KEY_AUTH_PLUGIN_REF).expect("resolved");

        registry.register(Arc::clone(&shared));
        let second = registry.resolve(API_KEY_AUTH_PLUGIN_REF).expect("resolved");

        assert!(Arc::ptr_eq(&first, &second), "the same plugin, replaced");
        assert_eq!(registry.len(), 1, "the map keeps one entry per identifier");
    }

    #[test]
    fn empty_registries_report_themselves_as_empty() {
        let registries = PluginRegistries::default();

        assert!(registries.auth.is_empty());
        assert!(registries.guard.is_empty());
        assert!(registries.transform.is_empty());
    }
}
