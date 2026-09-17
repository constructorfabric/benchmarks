//! In-process plugin registries (DESIGN "Resolution Algorithm").
//!
//! Named built-in plugins are resolved from these registries; UUID-backed
//! custom plugins are resolved from the control plane and executed through
//! the same trait surface. Catalog-only identifiers (`basic`, `bearer`,
//! `timeout`, `cors`, `logging`, `metrics`) are intentionally NOT resolvable.

use std::collections::HashMap;
use std::sync::Arc;

use crate::config::TokenCacheConfig;
use crate::domain::models::plugin_gts;
use crate::domain::plugin::{
    AuthPlugin, DynAuthPlugin, DynGuardPlugin, DynTransformPlugin, GuardPlugin, PluginError,
    TransformPlugin,
};

use super::apikey::ApiKeyAuthPlugin;
use super::noop::NoopAuthPlugin;
use super::oauth2_client_cred::{ClientAuthMethod, OAuth2ClientCredAuthPlugin};
use super::request_id::RequestIdTransformPlugin;
use super::required_headers::RequiredHeadersGuardPlugin;

/// Availability of a built-in plugin identifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuiltinPluginAvailability {
    /// Resolvable through the in-process registry.
    Resolvable,
    /// Catalog-only (types-registry only) — not bindable.
    CatalogOnly,
    /// Not a known identifier for this plugin family.
    Unknown,
}

/// Look up the availability of a plugin GTS identifier for a plugin family.
#[must_use]
pub fn classify_plugin(plugin_type: &str, gts_id: &str) -> BuiltinPluginAvailability {
    let instance = plugin_gts::instance_of(gts_id);
    match plugin_type {
        "auth" => match instance {
            "cf.core.oagw.noop.v1"
            | "cf.core.oagw.apikey.v1"
            | "cf.core.oagw.oauth2_client_cred.v1"
            | "cf.core.oagw.oauth2_client_cred_basic.v1" => BuiltinPluginAvailability::Resolvable,
            "cf.core.oagw.basic.v1" | "cf.core.oagw.bearer.v1" => {
                BuiltinPluginAvailability::CatalogOnly
            }
            _ => BuiltinPluginAvailability::Unknown,
        },
        "guard" => match instance {
            "cf.core.oagw.required_headers.v1" => BuiltinPluginAvailability::Resolvable,
            "cf.core.oagw.timeout.v1" | "cf.core.oagw.cors.v1" => {
                BuiltinPluginAvailability::CatalogOnly
            }
            _ => BuiltinPluginAvailability::Unknown,
        },
        "transform" => match instance {
            "cf.core.oagw.request_id.v1" => BuiltinPluginAvailability::Resolvable,
            "cf.core.oagw.logging.v1" | "cf.core.oagw.metrics.v1" => {
                BuiltinPluginAvailability::CatalogOnly
            }
            _ => BuiltinPluginAvailability::Unknown,
        },
        _ => BuiltinPluginAvailability::Unknown,
    }
}

/// Registry of built-in auth plugins.
#[derive(Clone)]
pub struct AuthPluginRegistry {
    plugins: Arc<HashMap<String, DynAuthPlugin>>,
}

impl AuthPluginRegistry {
    /// Create a registry with the built-in auth plugins (ADR-0008 wiring).
    #[must_use]
    pub fn with_builtins(
        credstore: Arc<dyn credstore_sdk::CredStoreClientV1>,
        token_cache: TokenCacheConfig,
        http_config: toolkit_http::HttpClientConfig,
    ) -> Self {
        let mut map: HashMap<String, DynAuthPlugin> = HashMap::new();
        let noop = Arc::new(NoopAuthPlugin);
        map.insert(noop.id().to_owned(), noop);

        let apikey = Arc::new(ApiKeyAuthPlugin::new(credstore.clone()));
        map.insert(apikey.id().to_owned(), apikey);

        let form = Arc::new(OAuth2ClientCredAuthPlugin::new(
            credstore.clone(),
            ClientAuthMethod::Form,
            token_cache,
            http_config.clone(),
        ));
        map.insert(form.id().to_owned(), form);

        let basic = Arc::new(OAuth2ClientCredAuthPlugin::new(
            credstore,
            ClientAuthMethod::Basic,
            token_cache,
            http_config,
        ));
        map.insert(basic.id().to_owned(), basic);

        Self {
            plugins: Arc::new(map),
        }
    }

    /// Resolve a named auth plugin by full GTS identifier.
    ///
    /// # Errors
    ///
    /// Returns `PluginError` when the identifier is unknown or catalog-only.
    pub fn resolve(&self, gts_id: &str) -> Result<DynAuthPlugin, PluginError> {
        self.plugins.get(gts_id).cloned().ok_or_else(|| {
            let availability = classify_plugin("auth", gts_id);
            let message = match availability {
                BuiltinPluginAvailability::CatalogOnly => {
                    format!("auth plugin {gts_id} is a catalog-only identifier with no backing implementation")
                }
                _ => format!("unknown auth plugin {gts_id}"),
            };
            PluginError::Config { message }
        })
    }
}

/// Registry of built-in guard plugins.
#[derive(Clone)]
pub struct GuardPluginRegistry {
    plugins: Arc<HashMap<String, DynGuardPlugin>>,
}

impl GuardPluginRegistry {
    /// Create a registry with the built-in guard plugins.
    #[must_use]
    pub fn with_builtins() -> Self {
        let mut map: HashMap<String, DynGuardPlugin> = HashMap::new();
        let required_headers = Arc::new(RequiredHeadersGuardPlugin);
        map.insert(required_headers.id().to_owned(), required_headers);
        Self {
            plugins: Arc::new(map),
        }
    }

    /// Resolve a named guard plugin by full GTS identifier.
    ///
    /// # Errors
    ///
    /// Returns `PluginError` when the identifier is unknown or catalog-only.
    pub fn resolve(&self, gts_id: &str) -> Result<DynGuardPlugin, PluginError> {
        self.plugins.get(gts_id).cloned().ok_or_else(|| {
            let availability = classify_plugin("guard", gts_id);
            let message = match availability {
                BuiltinPluginAvailability::CatalogOnly => {
                    format!("guard plugin {gts_id} is a catalog-only identifier (core Data Plane functionality)")
                }
                _ => format!("unknown guard plugin {gts_id}"),
            };
            PluginError::Config { message }
        })
    }
}

/// Registry of built-in transform plugins.
#[derive(Clone)]
pub struct TransformPluginRegistry {
    plugins: Arc<HashMap<String, DynTransformPlugin>>,
}

impl TransformPluginRegistry {
    /// Create a registry with the built-in transform plugins.
    #[must_use]
    pub fn with_builtins() -> Self {
        let mut map: HashMap<String, DynTransformPlugin> = HashMap::new();
        let request_id = Arc::new(RequestIdTransformPlugin);
        map.insert(request_id.id().to_owned(), request_id);
        Self {
            plugins: Arc::new(map),
        }
    }

    /// Resolve a named transform plugin by full GTS identifier.
    ///
    /// # Errors
    ///
    /// Returns `PluginError` when the identifier is unknown or catalog-only.
    pub fn resolve(&self, gts_id: &str) -> Result<DynTransformPlugin, PluginError> {
        self.plugins.get(gts_id).cloned().ok_or_else(|| {
            let availability = classify_plugin("transform", gts_id);
            let message = match availability {
                BuiltinPluginAvailability::CatalogOnly => {
                    format!("transform plugin {gts_id} is a catalog-only identifier (core Data Plane instrumentation)")
                }
                _ => format!("unknown transform plugin {gts_id}"),
            };
            PluginError::Config { message }
        })
    }
}
