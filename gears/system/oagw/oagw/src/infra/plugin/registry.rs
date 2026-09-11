//! The three plugin registries.
//!
//! Only built-in identifiers resolve here; a catalog-only identifier returns
//! `None`, which the data plane turns into a fail-closed error rather than a
//! silent pass-through.

use std::collections::BTreeMap;
use std::sync::Arc;

use crate::domain::error::DomainError;
use crate::domain::gts_helpers::is_catalog_only_plugin;
use crate::domain::plugin::{AuthPlugin, GuardPlugin, SecretResolver, TransformPlugin};
use crate::infra::plugin::apikey_auth::{ApiKeyAuthPlugin, NoopAuthPlugin};
use crate::infra::plugin::oauth2_client_cred_auth::{
    ClientCredentialMethod, OAuth2ClientCredAuthPlugin,
};
use crate::infra::plugin::request_id_transform::RequestIdTransformPlugin;
use crate::infra::plugin::required_headers_guard::RequiredHeadersGuardPlugin;

/// Auth-plugin registry.
pub struct AuthPluginRegistry {
    plugins: BTreeMap<&'static str, Arc<dyn AuthPlugin>>,
}

impl AuthPluginRegistry {
    /// Registers the built-in auth plugins.
    #[must_use]
    pub fn with_builtins(
        secrets: Arc<dyn SecretResolver>,
        cache_ttl: std::time::Duration,
        cache_capacity: usize,
    ) -> Self {
        let mut plugins: BTreeMap<&'static str, Arc<dyn AuthPlugin>> = BTreeMap::new();
        let noop = NoopAuthPlugin;
        plugins.insert(noop.id(), Arc::new(noop));
        let apikey = ApiKeyAuthPlugin::new(Arc::clone(&secrets));
        plugins.insert(apikey.id(), Arc::new(apikey));
        let form = OAuth2ClientCredAuthPlugin::new(
            Arc::clone(&secrets),
            ClientCredentialMethod::Form,
            cache_ttl,
            cache_capacity,
        );
        plugins.insert(form.id(), Arc::new(form));
        let basic = OAuth2ClientCredAuthPlugin::new(
            secrets,
            ClientCredentialMethod::Basic,
            cache_ttl,
            cache_capacity,
        );
        plugins.insert(basic.id(), Arc::new(basic));
        Self { plugins }
    }

    /// Resolves an auth plugin by GTS identifier.
    #[must_use]
    pub fn resolve(&self, id: &str) -> Option<Arc<dyn AuthPlugin>> {
        if is_catalog_only_plugin(id) {
            return None;
        }
        self.plugins.get(id).cloned()
    }

    /// Every registered identifier.
    #[must_use]
    pub fn ids(&self) -> Vec<&'static str> {
        self.plugins.keys().copied().collect()
    }
}

/// Guard-plugin registry.
#[derive(Default)]
#[allow(clippy::missing_fields_in_debug)]
pub struct GuardPluginRegistry {
    plugins: BTreeMap<&'static str, Arc<dyn GuardPlugin>>,
}

impl GuardPluginRegistry {
    /// Registers the built-in guard plugins.
    #[must_use]
    pub fn with_builtins() -> Self {
        let mut plugins: BTreeMap<&'static str, Arc<dyn GuardPlugin>> = BTreeMap::new();
        plugins.insert(
            RequiredHeadersGuardPlugin.id(),
            Arc::new(RequiredHeadersGuardPlugin),
        );
        Self { plugins }
    }

    /// Resolves a guard plugin by GTS identifier.
    #[must_use]
    pub fn resolve(&self, id: &str) -> Option<Arc<dyn GuardPlugin>> {
        if is_catalog_only_plugin(id) {
            return None;
        }
        self.plugins.get(id).cloned()
    }

    /// Every registered identifier.
    #[must_use]
    pub fn ids(&self) -> Vec<&'static str> {
        self.plugins.keys().copied().collect()
    }
}

/// Transform-plugin registry.
#[derive(Default)]
#[allow(clippy::missing_fields_in_debug)]
pub struct TransformPluginRegistry {
    plugins: BTreeMap<&'static str, Arc<dyn TransformPlugin>>,
}

impl TransformPluginRegistry {
    /// Registers the built-in transform plugins.
    #[must_use]
    pub fn with_builtins() -> Self {
        let mut plugins: BTreeMap<&'static str, Arc<dyn TransformPlugin>> = BTreeMap::new();
        plugins.insert(
            RequestIdTransformPlugin.id(),
            Arc::new(RequestIdTransformPlugin),
        );
        Self { plugins }
    }

    /// Resolves a transform plugin by GTS identifier.
    #[must_use]
    pub fn resolve(&self, id: &str) -> Option<Arc<dyn TransformPlugin>> {
        if is_catalog_only_plugin(id) {
            return None;
        }
        self.plugins.get(id).cloned()
    }

    /// Every registered identifier.
    #[must_use]
    pub fn ids(&self) -> Vec<&'static str> {
        self.plugins.keys().copied().collect()
    }
}

/// The error produced when a plugin identifier cannot be resolved.
///
/// # Errors
/// Returns [`DomainError::AuthenticationFailed`] for auth plugins and
/// [`DomainError::Validation`] for the other families.
#[must_use]
pub fn unresolvable_plugin_error(family: &str, id: &str) -> DomainError {
    match family {
        "auth" => DomainError::AuthenticationFailed(format!(
            "auth plugin `{id}` is not resolvable in this deployment"
        )),
        _ => DomainError::Validation(format!(
            "{family} plugin `{id}` is not resolvable in this deployment"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::plugin::NullSecretResolver;
    use crate::infra::plugin::oauth2_client_cred_auth::{DEFAULT_TOKEN_CACHE_CAPACITY, DEFAULT_TOKEN_CACHE_TTL};

    fn auth_registry() -> AuthPluginRegistry {
        AuthPluginRegistry::with_builtins(
            Arc::new(NullSecretResolver),
            DEFAULT_TOKEN_CACHE_TTL,
            DEFAULT_TOKEN_CACHE_CAPACITY,
        )
    }

    #[test]
    fn builtin_auth_plugins_resolve() {
        let registry = auth_registry();
        assert!(registry
            .resolve("gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.noop.v1")
            .is_some());
        assert!(registry
            .resolve("gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1")
            .is_some());
        assert!(registry
            .resolve("gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.oauth2_client_cred.v1")
            .is_some());
        assert!(registry
            .resolve("gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.oauth2_client_cred_basic.v1")
            .is_some());
    }

    #[test]
    fn catalog_only_auth_identifiers_resolve_to_nothing() {
        let registry = auth_registry();
        assert!(registry
            .resolve("gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.basic.v1")
            .is_none());
        assert!(registry
            .resolve("gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.bearer.v1")
            .is_none());
    }

    #[test]
    fn guard_and_transform_registries_only_admit_their_builtins() {
        let guards = GuardPluginRegistry::with_builtins();
        assert!(guards
            .resolve("gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1")
            .is_some());
        assert!(guards
            .resolve("gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.timeout.v1")
            .is_none());
        assert!(guards
            .resolve("gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.cors.v1")
            .is_none());

        let transforms = TransformPluginRegistry::with_builtins();
        assert!(transforms
            .resolve("gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.request_id.v1")
            .is_some());
        assert!(transforms
            .resolve("gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.logging.v1")
            .is_none());
        assert!(transforms
            .resolve("gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.metrics.v1")
            .is_none());
    }

    #[test]
    fn unresolvable_auth_plugin_is_an_authentication_failure() {
        let error = unresolvable_plugin_error(
            "auth",
            "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.bearer.v1",
        );
        assert_eq!(error.status(), 401);
    }
}
