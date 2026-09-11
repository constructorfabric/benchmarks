//! The three plugin registries (`cpt-cf-oagw-dod-plugin-system-registries`).
//!
//! Each registry is constructed through `with_builtins()`, registers the
//! built-in plugins of its type, and resolves a **named** plugin by its GTS
//! identifier. A registry returns an unresolvable outcome for any identifier
//! it does not carry, and never resolves a catalog-only identifier — the
//! catalog-only set is consulted by the binding-time check, not by a registry.
//!
//! The sandboxing surface for `cpt-cf-oagw-nfr-starlark-sandbox` (graded
//! deviation 6) is this trait boundary: a registry entry is a compiled Rust
//! object behind one of the three plugin traits, and there is no execution
//! path for a registered plugin's `source_code` content.

use std::collections::HashMap;
use std::sync::Arc;

use toolkit_auth::oauth2::ClientAuthMethod;
use toolkit_http::HttpClientConfig;

use crate::config::TokenCacheConfig;
use crate::domain::plugin::{AuthPlugin, GuardPlugin, TransformPlugin};
use crate::domain::type_catalog;
use crate::infra::plugin::apikey_auth::{ApiKeyAuthPlugin, NoopAuthPlugin};
use crate::infra::plugin::credentials::CredentialResolver;
use crate::infra::plugin::oauth2_client_cred_auth::OAuth2ClientCredAuthPlugin;
use crate::infra::plugin::required_headers_guard::RequiredHeadersGuardPlugin;
use crate::infra::plugin::request_id_transform::RequestIdTransformPlugin;

/// The lookup outcome of one registry resolution.
pub enum Resolution<T: ?Sized> {
    /// The registry carries the identifier.
    Found(Arc<T>),
    /// The registry carries no entry for the identifier.
    Unresolved,
}

impl<T: ?Sized> Resolution<T> {
    /// The plugin, or `None` when the identifier is unresolvable.
    #[must_use]
    pub fn ok(&self) -> Option<Arc<T>> {
        match self {
            Self::Found(plugin) => Some(Arc::clone(plugin)),
            Self::Unresolved => None,
        }
    }

    /// Whether the identifier resolved.
    #[must_use]
    pub const fn is_resolved(&self) -> bool {
        matches!(self, Self::Found(_))
    }
}

impl<T: ?Sized> Clone for Resolution<T> {
    fn clone(&self) -> Self {
        match self {
            Self::Found(plugin) => Self::Found(Arc::clone(plugin)),
            Self::Unresolved => Self::Unresolved,
        }
    }
}

impl<T: ?Sized> std::fmt::Debug for Resolution<T> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Found(_) => formatter.write_str("Found"),
            Self::Unresolved => formatter.write_str("Unresolved"),
        }
    }
}

/// Registry-indexed aliases for a built-in plugin.
///
/// A built-in is registered under three keys so both the FEATURE's short
/// labels (`noop`, `apikey`, …) and the GTS-identifier lookups resolve:
/// the label, the instance segment (`cf.core.oagw.noop.v1`), and the full
/// identifier.
fn aliases(label: &'static str, plugin_type: &'static str) -> [&'static str; 3] {
    [label, instance_segment_of(plugin_type), plugin_type]
}

/// The instance segment of a plugin GTS identifier, after the `~`.
#[must_use]
pub fn instance_segment_of(plugin_type: &str) -> &str {
    plugin_type.rsplit('~').next().unwrap_or(plugin_type)
}

macro_rules! registry {
    ($name:ident, $trait:ident, $doc:literal) => {
        #[doc = $doc]
        #[derive(Clone)]
        pub struct $name {
            entries: HashMap<String, Arc<dyn $trait>>,
        }

        impl $name {
            /// The identifiers the registry carries.
            #[must_use]
            pub fn ids(&self) -> Vec<String> {
                self.entries.keys().cloned().collect()
            }

            /// Whether the registry carries the identifier.
            #[must_use]
            pub fn contains(&self, identifier: &str) -> bool {
                self.entries.contains_key(identifier)
            }

            /// Resolve the identifier, or return an unresolvable outcome.
            ///
            /// A custom plugin is resolved by its UUID-backed record through
            /// the tenant-scoped repository; this lookup is the *named* half
            /// of `cpt-cf-oagw-algo-plugin-system-identifier-resolution`.
            #[must_use]
            pub fn get(&self, identifier: &str) -> Resolution<dyn $trait> {
                match self.entries.get(identifier) {
                    Some(plugin) => Resolution::Found(Arc::clone(plugin)),
                    None => Resolution::Unresolved,
                }
            }

            fn register(&mut self, label: &'static str, plugin_type: &'static str, plugin: Arc<dyn $trait>) {
                for alias in aliases(label, plugin_type) {
                    self.entries.insert(alias.to_owned(), Arc::clone(&plugin));
                }
            }
        }
    };
}

registry!(
    AuthPluginRegistry,
    AuthPlugin,
    "The authentication-plugin registry: the built-in `noop`, `apikey`, `oauth2_client_cred` and `oauth2_client_cred_basic` plugins."
);

registry!(
    GuardPluginRegistry,
    GuardPlugin,
    "The guard-plugin registry: the built-in `required_headers` guard, the only guard plugin with a backing implementation."
);

registry!(
    TransformPluginRegistry,
    TransformPlugin,
    "The transform-plugin registry: the built-in `request_id` transform."
);

impl AuthPluginRegistry {
    /// The registry carrying the four built-in auth plugins
    /// (`cpt-cf-oagw-dod-plugin-system-builtin-auth`).
    ///
    /// The two OAuth2 identifiers are two instances of one plugin that differ
    /// only in `auth_method` and **share one token cache**, which is why the
    /// cache is built here once and handed to both instances (ADR 0008).
    #[must_use]
    pub fn with_builtins(
        credstore: Arc<dyn credstore_sdk::api::CredStoreClientV1>,
        cache_config: TokenCacheConfig,
    ) -> Self {
        Self::with_http_config(credstore, cache_config, None)
    }

    /// [`Self::with_builtins`] with an explicit token-endpoint HTTP client
    /// configuration, which the unit tests use to point the exchange at a
    /// local server.
    #[must_use]
    pub fn with_http_config(
        credstore: Arc<dyn credstore_sdk::api::CredStoreClientV1>,
        cache_config: TokenCacheConfig,
        http_config: Option<HttpClientConfig>,
    ) -> Self {
        let resolver = CredentialResolver::new(credstore);
        // One cache for both variants (ADR 0008); the client-auth method is a
        // component of the cache key, so the two variants' entries stay apart
        // while sharing the one cache (`inst-ps-key-4`).
        let cache = OAuth2ClientCredAuthPlugin::new_token_cache(cache_config);
        let mut this = Self { entries: HashMap::new() };
        this.register("noop", crate::infra::plugin::apikey_auth::NOOP_PLUGIN_TYPE, Arc::new(NoopAuthPlugin));
        this.register(
            "apikey",
            crate::infra::plugin::apikey_auth::APIKEY_PLUGIN_TYPE,
            Arc::new(ApiKeyAuthPlugin::new(resolver.clone())),
        );
        this.register(
            "oauth2_client_cred",
            crate::infra::plugin::oauth2_client_cred_auth::OAUTH2_FORM_PLUGIN_TYPE,
            Arc::new(OAuth2ClientCredAuthPlugin::with_cache(
                crate::infra::plugin::oauth2_client_cred_auth::OAUTH2_FORM_PLUGIN_TYPE,
                ClientAuthMethod::Form,
                resolver.clone(),
                Arc::clone(&cache),
                cache_config,
                http_config.clone(),
            )),
        );
        this.register(
            "oauth2_client_cred_basic",
            crate::infra::plugin::oauth2_client_cred_auth::OAUTH2_BASIC_PLUGIN_TYPE,
            Arc::new(OAuth2ClientCredAuthPlugin::with_cache(
                crate::infra::plugin::oauth2_client_cred_auth::OAUTH2_BASIC_PLUGIN_TYPE,
                ClientAuthMethod::Basic,
                resolver,
                cache,
                cache_config,
                http_config,
            )),
        );
        this
    }
}

impl GuardPluginRegistry {
    /// The registry carrying the built-in required-headers guard.
    #[must_use]
    pub fn with_builtins() -> Self {
        let mut this = Self { entries: HashMap::new() };
        this.register(
            "required_headers",
            crate::infra::plugin::required_headers_guard::REQUIRED_HEADERS_PLUGIN_TYPE,
            Arc::new(RequiredHeadersGuardPlugin),
        );
        this
    }
}

impl TransformPluginRegistry {
    /// The registry carrying the built-in request-id transform.
    #[must_use]
    pub fn with_builtins() -> Self {
        let mut this = Self { entries: HashMap::new() };
        this.register(
            "request_id",
            crate::infra::plugin::request_id_transform::REQUEST_ID_PLUGIN_TYPE,
            Arc::new(RequestIdTransformPlugin),
        );
        this
    }
}

/// `cpt-cf-oagw-dod-plugin-system-identifier-resolution`: the config schema a
/// named plugin's binding is validated against.
///
/// A built-in resolves its schema from the catalog; a custom plugin resolves
/// its own `config_schema` from its `oagw_plugin` row.
#[must_use]
pub fn named_config_schema(identifier: &str) -> Option<serde_json::Value> {
    type_catalog::builtin_config_schema(identifier)
}

#[cfg(test)]
#[path = "registry_tests.rs"]
mod registry_tests;
