//! The three plugin registries (`cpt-cf-oagw-dod-plugin-registries`).
//!
//! One generic [`PluginRegistry`] keyed by the full named GTS identifier form
//! `gts.cf.core.oagw.{type}_plugin.v1~cf.core.oagw.{name}.v1`, so a binding's
//! `plugin_ref` resolves by direct lookup (§1.5). Registration is an init-time
//! operation: it needs `&mut self`, so a registry the gear holds behind
//! `OnceLock`/`Arc` exposes no registration path after init, and the last
//! registration at init wins, replacing an entry a built-in already holds — the
//! map-insertion pattern ADR 0002's loading example shows.
// @cpt-begin:cpt-cf-oagw-dod-plugin-registries:p1:inst-full

use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;

use credstore_sdk::CredStoreClientV1;
use toolkit_http::HttpClientConfig;

use crate::domain::plugin::{AuthPlugin, GuardPlugin, TransformPlugin};
use crate::infra::plugin::auth::{ApiKeyAuthPlugin, NoopAuthPlugin};
use crate::infra::plugin::guard::RequiredHeadersGuardPlugin;
use crate::infra::plugin::oauth2_client_cred_auth::OAuth2ClientCredAuthPlugin;
use crate::infra::plugin::token_cache::TokenCacheConfig;
use crate::infra::plugin::transform::RequestIdTransformPlugin;
use toolkit_auth::oauth2::ClientAuthMethod;

/// Full GTS identifier of the built-in `noop` auth plugin.
pub const NOOP_AUTH_PLUGIN_ID: &str = "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.noop.v1";
/// Full GTS identifier of the built-in `apikey` auth plugin.
pub const APIKEY_AUTH_PLUGIN_ID: &str = "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1";
/// Full GTS identifier of the built-in OAuth2 client-credentials auth plugin in
/// its `Form` variant.
pub const OAUTH2_CLIENT_CRED_AUTH_PLUGIN_ID: &str =
    "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.oauth2_client_cred.v1";
/// Full GTS identifier of the built-in OAuth2 client-credentials auth plugin in
/// its `Basic` variant.
pub const OAUTH2_CLIENT_CRED_BASIC_AUTH_PLUGIN_ID: &str =
    "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.oauth2_client_cred_basic.v1";
/// Full GTS identifier of the built-in `required_headers` guard plugin.
pub const REQUIRED_HEADERS_GUARD_PLUGIN_ID: &str =
    "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1";
/// Full GTS identifier of the built-in `request_id` transform plugin.
pub const REQUEST_ID_TRANSFORM_PLUGIN_ID: &str =
    "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.request_id.v1";

/// The registry of the auth plugins, keyed by the full named GTS identifier.
pub type AuthPluginRegistry = PluginRegistry<dyn AuthPlugin>;
/// The registry of the guard plugins, keyed by the full named GTS identifier.
pub type GuardPluginRegistry = PluginRegistry<dyn GuardPlugin>;
/// The registry of the transform plugins, keyed by the full named GTS
/// identifier.
pub type TransformPluginRegistry = PluginRegistry<dyn TransformPlugin>;

/// The registration seam the registries key an entry by: the identifier the
/// plugin itself declares, which every plugin trait exposes as `id()`.
///
/// Implemented for the three trait-object types the registries hold, so one
/// generic [`PluginRegistry::register`] serves all three families and no plugin
/// can be registered under an identifier other than the one it declares.
pub trait RegisteredPlugin {
    /// The full GTS identifier the plugin is registered under.
    fn plugin_id(&self) -> &str;
}

impl RegisteredPlugin for dyn AuthPlugin {
    fn plugin_id(&self) -> &str {
        AuthPlugin::id(self)
    }
}

impl RegisteredPlugin for dyn GuardPlugin {
    fn plugin_id(&self) -> &str {
        GuardPlugin::id(self)
    }
}

impl RegisteredPlugin for dyn TransformPlugin {
    fn plugin_id(&self) -> &str {
        TransformPlugin::id(self)
    }
}

/// One `{type}_plugin` registry: a map from the full named GTS identifier to
/// the executable plugin.
pub struct PluginRegistry<T: ?Sized + 'static> {
    entries: HashMap<String, Arc<T>>,
}

impl<T: ?Sized + 'static> Clone for PluginRegistry<T> {
    fn clone(&self) -> Self {
        Self {
            entries: self.entries.clone(),
        }
    }
}

impl<T: ?Sized + 'static> PluginRegistry<T> {
    /// Creates an empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self {
            entries: HashMap::new(),
        }
    }

    /// Resolves a binding's `plugin_ref` by direct lookup of the whole
    /// full-named-identifier string.
    ///
    /// No registry contains any of the six catalog-only identifiers, so a
    /// lookup with one of them fails exactly like a lookup with an unknown
    /// identifier, and a reference of another `{type}_plugin` family is absent
    /// from this registry by construction.
    #[must_use]
    pub fn lookup(&self, plugin_ref: &str) -> Option<Arc<T>> {
        self.entries.get(plugin_ref).cloned()
    }

    /// The number of registered plugins.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the registry holds no plugin.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

impl<T: ?Sized + RegisteredPlugin + 'static> PluginRegistry<T> {
    /// Registers one plugin under the identifier it declares.
    ///
    /// Init-time only: the last registration at init wins, replacing a
    /// duplicate identifier, which is how an external ToolKit plugin shadows a
    /// built-in (ADR 0002 map insertion). No registration path exists after
    /// init and none is reachable from a request.
    pub fn register(&mut self, plugin: Arc<T>) {
        // @cpt-begin:cpt-cf-oagw-flow-registry-init:p1:inst-pi-09
        // The external plugins a ToolKit gear supplies at init are registered
        // into the same registry, identically to the built-ins, under the
        // identifier the plugin itself declares; a duplicate identifier
        // replaces the previous entry.
        self.entries.insert(plugin.plugin_id().to_owned(), plugin);
        // @cpt-end:cpt-cf-oagw-flow-registry-init:p1:inst-pi-09
    }

    /// The identifiers the registry holds, sorted for deterministic
    /// diagnostics.
    #[must_use]
    pub fn ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = self.entries.keys().cloned().collect();
        ids.sort_unstable();
        ids
    }
}

impl<T: ?Sized + 'static> fmt::Debug for PluginRegistry<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PluginRegistry")
            .field("entries", &self.entries.len())
            .finish()
    }
}

impl Default for AuthPluginRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl Default for GuardPluginRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl Default for TransformPluginRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl AuthPluginRegistry {
    /// Registers the four built-in auth plugins under their full named GTS
    /// identifiers.
    ///
    /// The `cred_store` client, ADR 0008's `token_http_config` parameter and
    /// the [`TokenCacheConfig`] parameter object are threaded into the two
    /// OAuth2 client-credentials constructors, so the cache the plugins own is
    /// sized and bounded by gear configuration.
    #[must_use]
    pub fn with_builtins(
        cred_store: Arc<dyn CredStoreClientV1>,
        token_http_config: Option<HttpClientConfig>,
        token_cache_config: TokenCacheConfig,
    ) -> Self {
        let mut registry = Self::new();
        // @cpt-begin:cpt-cf-oagw-flow-registry-init:p1:inst-pi-06
        // The four auth built-ins: noop, apikey, and the two OAuth2
        // client-credentials variants sharing one implementation and one cache
        // configuration, registered under the `Form` and the `Basic` id.
        registry.register(Arc::new(NoopAuthPlugin));
        registry.register(Arc::new(ApiKeyAuthPlugin::new(Arc::clone(&cred_store))));
        registry.register(Arc::new(OAuth2ClientCredAuthPlugin::new(
            Arc::clone(&cred_store),
            ClientAuthMethod::Form,
            token_http_config.clone(),
            token_cache_config,
        )));
        registry.register(Arc::new(OAuth2ClientCredAuthPlugin::new(
            cred_store,
            ClientAuthMethod::Basic,
            token_http_config,
            token_cache_config,
        )));
        // @cpt-end:cpt-cf-oagw-flow-registry-init:p1:inst-pi-06
        registry
    }
}

impl GuardPluginRegistry {
    /// Registers `required_headers`, the only built-in guard plugin
    /// (ADR 0009).
    #[must_use]
    pub fn with_builtins() -> Self {
        let mut registry = Self::new();
        // @cpt-begin:cpt-cf-oagw-flow-registry-init:p1:inst-pi-07
        // The single guard built-in; timeout and CORS are catalog-only
        // identifiers that no registry contains.
        registry.register(Arc::new(RequiredHeadersGuardPlugin));
        // @cpt-end:cpt-cf-oagw-flow-registry-init:p1:inst-pi-07
        registry
    }
}

impl TransformPluginRegistry {
    /// Registers `request_id`, the only built-in transform plugin.
    #[must_use]
    pub fn with_builtins() -> Self {
        let mut registry = Self::new();
        // @cpt-begin:cpt-cf-oagw-flow-registry-init:p1:inst-pi-08
        // The one transform built-in; logging and metrics are core data-plane
        // instrumentation and stay catalog-only identifiers.
        registry.register(Arc::new(RequestIdTransformPlugin));
        // @cpt-end:cpt-cf-oagw-flow-registry-init:p1:inst-pi-08
        registry
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::plugin::{AUTH_PLUGIN_TYPE, GUARD_PLUGIN_TYPE, TRANSFORM_PLUGIN_TYPE};
    use credstore_sdk::test_util::MockCredStoreClient;
    use std::sync::OnceLock;

    /// The six catalog-only identifiers of §1.5: declared in the types-registry
    /// catalog, absent from every registry. They exist as test fixtures only.
    const CATALOG_ONLY_AUTH_IDS: [&str; 2] = [
        "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.basic.v1",
        "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.bearer.v1",
    ];
    const CATALOG_ONLY_GUARD_IDS: [&str; 2] = [
        "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.timeout.v1",
        "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.cors.v1",
    ];
    const CATALOG_ONLY_TRANSFORM_IDS: [&str; 2] = [
        "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.logging.v1",
        "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.metrics.v1",
    ];

    fn token_cache_config() -> TokenCacheConfig {
        TokenCacheConfig::new(300, 128)
    }

    fn auth_registry() -> AuthPluginRegistry {
        AuthPluginRegistry::with_builtins(
            Arc::new(MockCredStoreClient::empty()),
            None,
            token_cache_config(),
        )
    }

    /// An external auth plugin registering under the identifier it declares,
    /// the shape ADR 0002's external-plugin example shows.
    struct ExternalAuthPlugin;

    #[async_trait::async_trait]
    impl AuthPlugin for ExternalAuthPlugin {
        fn id(&self) -> &str {
            "gts.cf.core.oagw.auth_plugin.v1~custom.oauth2.oagw.pkce.v1"
        }
        fn plugin_type(&self) -> &str {
            AUTH_PLUGIN_TYPE
        }
        async fn authenticate(
            &self,
            _ctx: &mut crate::domain::plugin::RequestContext,
        ) -> Result<(), crate::domain::error::OagwError> {
            Ok(())
        }
    }

    #[test]
    fn the_four_auth_builtins_resolve_under_their_full_gts_form() {
        let registry = auth_registry();

        assert_eq!(registry.len(), 4);
        for id in [
            NOOP_AUTH_PLUGIN_ID,
            APIKEY_AUTH_PLUGIN_ID,
            OAUTH2_CLIENT_CRED_AUTH_PLUGIN_ID,
            OAUTH2_CLIENT_CRED_BASIC_AUTH_PLUGIN_ID,
        ] {
            let plugin = registry
                .lookup(id)
                .unwrap_or_else(|| panic!("{id} must resolve under its full GTS form"));
            assert_eq!(plugin.id(), id);
            assert_eq!(plugin.plugin_type(), AUTH_PLUGIN_TYPE);
        }
    }

    #[test]
    fn the_guard_and_transform_builtins_resolve_under_their_full_gts_form() {
        let guard = GuardPluginRegistry::with_builtins();
        let required_headers = guard
            .lookup(REQUIRED_HEADERS_GUARD_PLUGIN_ID)
            .expect("the required_headers guard resolves");
        assert_eq!(guard.len(), 1);
        assert_eq!(required_headers.id(), REQUIRED_HEADERS_GUARD_PLUGIN_ID);
        assert_eq!(required_headers.plugin_type(), GUARD_PLUGIN_TYPE);

        let transform = TransformPluginRegistry::with_builtins();
        let request_id = transform
            .lookup(REQUEST_ID_TRANSFORM_PLUGIN_ID)
            .expect("the request_id transform resolves");
        assert_eq!(transform.len(), 1);
        assert_eq!(request_id.id(), REQUEST_ID_TRANSFORM_PLUGIN_ID);
        assert_eq!(request_id.plugin_type(), TRANSFORM_PLUGIN_TYPE);
    }

    #[test]
    fn each_catalog_only_identifier_is_unresolvable_in_its_family() {
        let auth = auth_registry();
        for id in CATALOG_ONLY_AUTH_IDS {
            assert!(auth.lookup(id).is_none(), "{id} must not resolve");
        }

        let guard = GuardPluginRegistry::with_builtins();
        for id in CATALOG_ONLY_GUARD_IDS {
            assert!(guard.lookup(id).is_none(), "{id} must not resolve");
        }

        let transform = TransformPluginRegistry::with_builtins();
        for id in CATALOG_ONLY_TRANSFORM_IDS {
            assert!(transform.lookup(id).is_none(), "{id} must not resolve");
        }
    }

    #[test]
    fn an_unknown_identifier_and_a_catalog_only_one_fail_identically() {
        let registry = auth_registry();
        let unknown = registry.lookup("gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.unknown.v1");
        let catalog_only = registry.lookup(CATALOG_ONLY_AUTH_IDS[0]);

        assert!(unknown.is_none());
        assert!(catalog_only.is_none());
        assert_eq!(
            registry.lookup(CATALOG_ONLY_AUTH_IDS[1]).is_none(),
            registry
                .lookup("gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.unknown.v1")
                .is_none(),
            "a catalog-only identifier is not distinguishable from an unknown one"
        );
    }

    #[test]
    fn a_uuid_backed_reference_resolves_in_no_registry() {
        let uuid_ref = "gts.cf.core.oagw.auth_plugin.v1~3f0a1b2c-3d4e-4f50-8617-8899aabbccdd";
        let bare_uuid = "3f0a1b2c-3d4e-4f50-8617-8899aabbccdd";

        assert!(auth_registry().lookup(uuid_ref).is_none());
        assert!(
            GuardPluginRegistry::with_builtins()
                .lookup(uuid_ref)
                .is_none()
        );
        assert!(
            TransformPluginRegistry::with_builtins()
                .lookup(bare_uuid)
                .is_none()
        );
    }

    #[test]
    fn an_external_plugin_registers_under_the_identifier_it_declares() {
        let mut registry = auth_registry();
        registry.register(Arc::new(ExternalAuthPlugin) as Arc<dyn AuthPlugin>);

        let external = registry
            .lookup("gts.cf.core.oagw.auth_plugin.v1~custom.oauth2.oagw.pkce.v1")
            .expect("the external plugin resolves under its own id");
        assert_eq!(
            external.id(),
            "gts.cf.core.oagw.auth_plugin.v1~custom.oauth2.oagw.pkce.v1"
        );
        assert_eq!(
            registry.len(),
            5,
            "one external plugin joins the four built-ins"
        );
    }

    #[test]
    fn an_external_plugin_registered_over_a_builtin_id_replaces_it() {
        /// Shadows `noop`: the last registration at init wins.
        struct ShadowingAuthPlugin;

        #[async_trait::async_trait]
        impl AuthPlugin for ShadowingAuthPlugin {
            fn id(&self) -> &str {
                NOOP_AUTH_PLUGIN_ID
            }
            fn plugin_type(&self) -> &str {
                AUTH_PLUGIN_TYPE
            }
            async fn authenticate(
                &self,
                _ctx: &mut crate::domain::plugin::RequestContext,
            ) -> Result<(), crate::domain::error::OagwError> {
                Ok(())
            }
        }

        let shadowing: Arc<dyn AuthPlugin> = Arc::new(ShadowingAuthPlugin);
        let mut registry = auth_registry();
        registry.register(Arc::clone(&shadowing));

        let resolved = registry
            .lookup(NOOP_AUTH_PLUGIN_ID)
            .expect("the identifier still resolves");
        assert_eq!(resolved.id(), NOOP_AUTH_PLUGIN_ID);
        assert!(
            Arc::ptr_eq(&resolved, &shadowing),
            "the shadowing plugin is what the identifier resolves to, \
             so the built-in is no longer resolvable"
        );
        assert_eq!(registry.len(), 4, "the entry was replaced, not added");
        assert_eq!(
            registry.ids(),
            vec![
                APIKEY_AUTH_PLUGIN_ID.to_owned(),
                NOOP_AUTH_PLUGIN_ID.to_owned(),
                OAUTH2_CLIENT_CRED_AUTH_PLUGIN_ID.to_owned(),
                OAUTH2_CLIENT_CRED_BASIC_AUTH_PLUGIN_ID.to_owned(),
            ]
        );
    }

    #[test]
    fn a_shared_registry_has_no_registration_path() {
        // The gear holds the registries behind a `OnceLock<PluginRegistries>`;
        // through the shared reference only the read surface is reachable.
        let shared: Arc<GuardPluginRegistry> = Arc::new(GuardPluginRegistry::with_builtins());
        let frozen: OnceLock<GuardPluginRegistry> = {
            let lock = OnceLock::new();
            let mut owned = GuardPluginRegistry::with_builtins();
            owned.register(Arc::new(RequiredHeadersGuardPlugin));
            assert!(lock.set(owned).is_ok());
            lock
        };

        assert_eq!(
            shared
                .lookup(REQUIRED_HEADERS_GUARD_PLUGIN_ID)
                .expect("the built-in resolves through the shared registry")
                .id(),
            REQUIRED_HEADERS_GUARD_PLUGIN_ID
        );
        assert_eq!(shared.len(), 1);
        assert!(!shared.is_empty());
        assert_eq!(
            frozen.get().map(PluginRegistry::len),
            Some(1),
            "`register` needs `&mut self`, so a registry behind `OnceLock`/`Arc` \
             offers no post-init registration path"
        );
        assert_eq!(
            shared.ids(),
            vec![REQUIRED_HEADERS_GUARD_PLUGIN_ID.to_owned()]
        );
    }

    #[test]
    fn an_empty_registry_reports_no_entries() {
        let registry = AuthPluginRegistry::new();
        assert!(registry.is_empty());
        assert_eq!(registry.len(), 0);
        assert!(registry.ids().is_empty());
    }
}

// @cpt-end:cpt-cf-oagw-dod-plugin-registries:p1:inst-full
