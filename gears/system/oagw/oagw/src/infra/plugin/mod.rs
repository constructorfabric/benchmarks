//! Built-in plugin implementations and the plugin registries (ADR-0002).

pub mod apikey_auth;
pub mod noop_auth;
pub mod oauth2_client_cred_auth;
pub mod request_id_transform;
pub mod required_headers_guard;

use std::collections::HashMap;
use std::sync::Arc;

use crate::domain::error::{OagwError, OagwResult};
use crate::domain::gts_helpers;
use crate::domain::plugin::{AuthPlugin, GuardPlugin, TransformPlugin};

/// Registry of auth plugins, keyed by GTS plugin id.
#[derive(Clone)]
pub struct AuthPluginRegistry {
    plugins: Arc<std::collections::HashMap<String, Arc<dyn AuthPlugin>>>,
}

impl std::fmt::Debug for AuthPluginRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuthPluginRegistry")
            .field("ids", &self.ids())
            .finish()
    }
}

impl AuthPluginRegistry {
    /// Registry with the built-in auth plugins (ADR-0002/0008).
    #[must_use]
    pub fn with_builtins(
        credstore: Option<Arc<dyn credstore_sdk::CredStoreClientV1>>,
        token_cache: crate::config::TokenCacheConfig,
    ) -> Self {
        let mut plugins: std::collections::HashMap<String, Arc<dyn AuthPlugin>> =
            std::collections::HashMap::new();
        plugins.insert(
            gts_helpers::NOOP_AUTH_PLUGIN_ID.to_owned(),
            Arc::new(noop_auth::NoopAuthPlugin),
        );
        plugins.insert(
            gts_helpers::APIKEY_AUTH_PLUGIN_ID.to_owned(),
            Arc::new(apikey_auth::ApiKeyAuthPlugin::new(credstore.clone())),
        );
        plugins.insert(
            gts_helpers::OAUTH2_CLIENT_CRED_PLUGIN_ID.to_owned(),
            Arc::new(oauth2_client_cred_auth::OAuth2ClientCredAuthPlugin::new(
                credstore.clone(),
                oauth2_client_cred_auth::Variant::Form,
                token_cache.ttl(),
                token_cache.capacity,
            )),
        );
        plugins.insert(
            gts_helpers::OAUTH2_CLIENT_CRED_BASIC_PLUGIN_ID.to_owned(),
            Arc::new(oauth2_client_cred_auth::OAuth2ClientCredAuthPlugin::new(
                credstore,
                oauth2_client_cred_auth::Variant::Basic,
                token_cache.ttl(),
                token_cache.capacity,
            )),
        );
        Self {
            plugins: Arc::new(plugins),
        }
    }

    /// Resolves a plugin by id.
    ///
    /// # Errors
    ///
    /// [`OagwError::PluginNotFound`] for unknown ids, and for catalogued ids
    /// that have no implementation (ADR-0002 "Built-in Plugins").
    pub fn get(&self, id: &str) -> OagwResult<Arc<dyn AuthPlugin>> {
        self.plugins
            .get(id)
            .cloned()
            .ok_or_else(|| plugin_not_found(id))
    }

    /// Registered plugin ids.
    #[must_use]
    pub fn ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = self.plugins.keys().cloned().collect();
        ids.sort();
        ids
    }
}

/// Registry of guard plugins.
#[derive(Clone)]
pub struct GuardPluginRegistry {
    plugins: Arc<std::collections::HashMap<String, Arc<dyn GuardPlugin>>>,
}

impl std::fmt::Debug for GuardPluginRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GuardPluginRegistry")
            .field("ids", &self.ids())
            .finish()
    }
}

impl GuardPluginRegistry {
    /// Registry with the built-in guard plugins (ADR-0009).
    #[must_use]
    pub fn with_builtins() -> Self {
        let mut plugins: std::collections::HashMap<String, Arc<dyn GuardPlugin>> =
            std::collections::HashMap::new();
        plugins.insert(
            gts_helpers::REQUIRED_HEADERS_GUARD_PLUGIN_ID.to_owned(),
            Arc::new(required_headers_guard::RequiredHeadersGuardPlugin),
        );
        Self {
            plugins: Arc::new(plugins),
        }
    }

    /// Resolves a plugin by id.
    ///
    /// # Errors
    ///
    /// [`OagwError::PluginNotFound`].
    pub fn get(&self, id: &str) -> OagwResult<Arc<dyn GuardPlugin>> {
        self.plugins
            .get(id)
            .cloned()
            .ok_or_else(|| plugin_not_found(id))
    }

    /// Registered plugin ids.
    #[must_use]
    pub fn ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = self.plugins.keys().cloned().collect();
        ids.sort();
        ids
    }
}

/// Registry of transform plugins.
#[derive(Clone)]
pub struct TransformPluginRegistry {
    plugins: Arc<std::collections::HashMap<String, Arc<dyn TransformPlugin>>>,
}

impl std::fmt::Debug for TransformPluginRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TransformPluginRegistry")
            .field("ids", &self.ids())
            .finish()
    }
}

impl TransformPluginRegistry {
    /// Registry with the built-in transform plugins.
    #[must_use]
    pub fn with_builtins() -> Self {
        let mut plugins: std::collections::HashMap<String, Arc<dyn TransformPlugin>> =
            std::collections::HashMap::new();
        plugins.insert(
            gts_helpers::REQUEST_ID_TRANSFORM_PLUGIN_ID.to_owned(),
            Arc::new(request_id_transform::RequestIdTransformPlugin),
        );
        Self {
            plugins: Arc::new(plugins),
        }
    }

    /// Resolves a plugin by id.
    ///
    /// # Errors
    ///
    /// [`OagwError::PluginNotFound`].
    pub fn get(&self, id: &str) -> OagwResult<Arc<dyn TransformPlugin>> {
        self.plugins
            .get(id)
            .cloned()
            .ok_or_else(|| plugin_not_found(id))
    }

    /// Registered plugin ids.
    #[must_use]
    pub fn ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = self.plugins.keys().cloned().collect();
        ids.sort();
        ids
    }
}

fn plugin_not_found(id: &str) -> OagwError {
    if gts_helpers::CATALOG_ONLY_PLUGIN_IDS.contains(&id) {
        OagwError::PluginNotFound(format!(
            "plugin '{id}' is catalogued but has no runtime implementation"
        ))
    } else {
        OagwError::PluginNotFound(format!("plugin '{id}' is not registered"))
    }
}

/// All plugin ids across the three registries, for `GET /plugins`.
#[must_use]
pub fn builtin_plugin_ids() -> Vec<String> {
    let mut ids: Vec<String> = Vec::new();
    ids.extend(gts_helpers::CATALOG_ONLY_PLUGIN_IDS.iter().map(|s| (*s).to_owned()));
    ids.push(gts_helpers::NOOP_AUTH_PLUGIN_ID.to_owned());
    ids.push(gts_helpers::APIKEY_AUTH_PLUGIN_ID.to_owned());
    ids.push(gts_helpers::OAUTH2_CLIENT_CRED_PLUGIN_ID.to_owned());
    ids.push(gts_helpers::OAUTH2_CLIENT_CRED_BASIC_PLUGIN_ID.to_owned());
    ids.push(gts_helpers::REQUIRED_HEADERS_GUARD_PLUGIN_ID.to_owned());
    ids.push(gts_helpers::REQUEST_ID_TRANSFORM_PLUGIN_ID.to_owned());
    ids.sort();
    ids
}

/// Convenience: empty map used by tests building custom registries.
#[must_use]
pub fn empty_registry() -> std::collections::HashMap<String, Arc<dyn AuthPlugin>> {
    HashMap::new()
}

#[cfg(test)]
pub(crate) mod test_support {
    use bytes::Bytes;
    use http::HeaderMap;
    use toolkit_security::SecurityContext;
    use uuid::Uuid;

    use crate::domain::plugin::RequestContext;

    /// A minimal `RequestContext` for plugin unit tests.
    #[must_use]
    pub fn request_context() -> RequestContext {
        RequestContext {
            security_context: SecurityContext::anonymous(),
            tenant_id: Uuid::new_v4(),
            upstream_id: Uuid::new_v4(),
            route_id: None,
            method: http::Method::GET,
            path: "/v1/resource".to_owned(),
            query: Vec::new(),
            headers: HeaderMap::new(),
            body: Bytes::new(),
            config: serde_json::Value::Null,
        }
    }
}
