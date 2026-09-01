//! Per-family plugin registries (builtins + custom plugin existence).
//!
//! `resolve` returns an executable builtin (custom plugin execution is not
//! implemented — Starlark sandboxing is out of scope) while `known_for`
//! reports whether a reference is bindable at all: a builtin id, or a custom
//! plugin persisted under the caller's tenant with the matching family.

use std::sync::Arc;

use uuid::Uuid;

use crate::config::TokenCacheConfig;
use crate::domain::dto::Plugin;
use crate::domain::gts_helpers::plugin_uuid_from_id;
use crate::domain::plugin::{AuthPlugin, GuardPlugin, TransformPlugin};
use crate::domain::repo::PluginRepository;

use super::apikey_auth::ApiKeyAuthPlugin;
use super::noop_auth::NoopAuthPlugin;
use super::oauth2_client_cred_auth::{OAuth2ClientCredAuthPlugin, OAuth2Flavor};
use super::request_id_transform::RequestIdTransformPlugin;
use super::required_headers_guard::RequiredHeadersGuardPlugin;

/// Shared custom-plugin existence lookup.
fn custom_known(
    plugin_repo: &dyn PluginRepository,
    plugin_ref: &str,
    tenant_id: Uuid,
    family: &str,
) -> bool {
    let Some(uuid) = plugin_uuid_from_id(plugin_ref) else {
        return false;
    };
    plugin_repo
        .get(tenant_id, uuid)
        .is_some_and(|p| p.plugin_type == family)
}

/// Registry of `AuthPlugin`s.
pub struct AuthPluginRegistry {
    plugin_repo: Arc<dyn PluginRepository>,
    builtins: Vec<Arc<dyn AuthPlugin>>,
}

impl AuthPluginRegistry {
    /// Empty registry (custom plugins only) — for tests.
    #[must_use]
    pub fn new(plugin_repo: Arc<dyn PluginRepository>) -> Self {
        Self {
            plugin_repo,
            builtins: Vec::new(),
        }
    }

    /// Registry with all built-in auth plugins (ADR-0008).
    #[must_use]
    pub fn with_builtins(
        credstore: Arc<dyn credstore_sdk::CredStoreClientV1>,
        token_http_config: Option<toolkit_http::HttpClientConfig>,
        token_cache_config: TokenCacheConfig,
        plugin_repo: Arc<dyn PluginRepository>,
    ) -> Self {
        let mut registry = Self::new(plugin_repo);
        registry.builtins.push(Arc::new(NoopAuthPlugin));
        registry
            .builtins
            .push(Arc::new(ApiKeyAuthPlugin::new(credstore.clone())));
        registry.builtins.push(Arc::new(
            OAuth2ClientCredAuthPlugin::new(
                OAuth2Flavor::Form,
                credstore.clone(),
                token_cache_config.ttl(),
                token_cache_config.capacity,
            )
            .with_http_config(token_http_config.clone()),
        ));
        registry.builtins.push(Arc::new(
            OAuth2ClientCredAuthPlugin::new(
                OAuth2Flavor::Basic,
                credstore,
                token_cache_config.ttl(),
                token_cache_config.capacity,
            )
            .with_http_config(token_http_config),
        ));
        registry
    }

    /// Register an additional builtin (e.g. in tests).
    #[must_use]
    pub fn hook(mut self, plugin: Arc<dyn AuthPlugin>) -> Self {
        self.builtins.push(plugin);
        self
    }

    /// Resolve a plugin reference to an executable plugin.
    #[must_use]
    pub fn resolve(&self, plugin_ref: &str) -> Option<Arc<dyn AuthPlugin>> {
        self.builtins.iter().find(|p| p.id() == plugin_ref).cloned()
    }

    /// Whether the reference is bindable as an auth plugin for `tenant_id`
    /// (builtin or a persisted custom auth plugin).
    #[must_use]
    pub fn known_for(&self, plugin_ref: &str, tenant_id: Uuid) -> bool {
        if self.builtins.iter().any(|p| p.id() == plugin_ref) {
            return true;
        }
        custom_known(self.plugin_repo.as_ref(), plugin_ref, tenant_id, "auth")
    }

    /// Custom plugin (for tenant-scoped binding resolution).
    #[must_use]
    pub fn custom(&self, tenant_id: Uuid, uuid: Uuid) -> Option<Plugin> {
        self.plugin_repo.get(tenant_id, uuid)
    }
}

/// Registry of `GuardPlugin`s.
pub struct GuardPluginRegistry {
    plugin_repo: Arc<dyn PluginRepository>,
    builtins: Vec<Arc<dyn GuardPlugin>>,
}

impl GuardPluginRegistry {
    /// Empty registry — for tests.
    #[must_use]
    pub fn new(plugin_repo: Arc<dyn PluginRepository>) -> Self {
        Self {
            plugin_repo,
            builtins: Vec::new(),
        }
    }

    /// Registry with the built-in `required_headers` guard.
    #[must_use]
    pub fn with_builtins(plugin_repo: Arc<dyn PluginRepository>) -> Self {
        let mut registry = Self::new(plugin_repo);
        registry.builtins.push(Arc::new(RequiredHeadersGuardPlugin));
        registry
    }

    /// Register an additional builtin.
    #[must_use]
    pub fn hook(mut self, plugin: Arc<dyn GuardPlugin>) -> Self {
        self.builtins.push(plugin);
        self
    }

    /// Resolve a plugin reference to an executable plugin.
    #[must_use]
    pub fn resolve(&self, plugin_ref: &str) -> Option<Arc<dyn GuardPlugin>> {
        self.builtins.iter().find(|p| p.id() == plugin_ref).cloned()
    }

    /// Whether the reference is bindable as a guard plugin for `tenant_id`.
    #[must_use]
    pub fn known_for(&self, plugin_ref: &str, tenant_id: Uuid) -> bool {
        if self.builtins.iter().any(|p| p.id() == plugin_ref) {
            return true;
        }
        custom_known(self.plugin_repo.as_ref(), plugin_ref, tenant_id, "guard")
    }
}

/// Registry of `TransformPlugin`s.
pub struct TransformPluginRegistry {
    plugin_repo: Arc<dyn PluginRepository>,
    builtins: Vec<Arc<dyn TransformPlugin>>,
}

impl TransformPluginRegistry {
    /// Empty registry — for tests.
    #[must_use]
    pub fn new(plugin_repo: Arc<dyn PluginRepository>) -> Self {
        Self {
            plugin_repo,
            builtins: Vec::new(),
        }
    }

    /// Registry with the built-in `request_id` transform.
    #[must_use]
    pub fn with_builtins(plugin_repo: Arc<dyn PluginRepository>) -> Self {
        let mut registry = Self::new(plugin_repo);
        registry.builtins.push(Arc::new(RequestIdTransformPlugin));
        registry
    }

    /// Register an additional builtin.
    #[must_use]
    pub fn hook(mut self, plugin: Arc<dyn TransformPlugin>) -> Self {
        self.builtins.push(plugin);
        self
    }

    /// Resolve a plugin reference to an executable plugin.
    #[must_use]
    pub fn resolve(&self, plugin_ref: &str) -> Option<Arc<dyn TransformPlugin>> {
        self.builtins.iter().find(|p| p.id() == plugin_ref).cloned()
    }

    /// Whether the reference is bindable as a transform plugin for
    /// `tenant_id`.
    #[must_use]
    pub fn known_for(&self, plugin_ref: &str, tenant_id: Uuid) -> bool {
        if self.builtins.iter().any(|p| p.id() == plugin_ref) {
            return true;
        }
        custom_known(
            self.plugin_repo.as_ref(),
            plugin_ref,
            tenant_id,
            "transform",
        )
    }
}
