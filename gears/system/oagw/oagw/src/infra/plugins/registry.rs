//! Plugin registries (`ADR/0002` § "Plugin Registry").
//!
//! Three registries — one per plugin type — keyed by the full GTS instance id.
//! Catalog-only identifiers are deliberately absent, so looking one up at
//! proxy time yields [`DomainError::PluginNotFound`].

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use crate::domain::error::DomainError;
use crate::domain::plugin::{AuthPlugin, GuardPlugin, TransformPlugin};
use crate::infra::plugins::apikey_auth::ApiKeyAuthPlugin;
use crate::infra::plugins::noop_auth::NoopAuthPlugin;
use crate::infra::plugins::oauth2_client_cred_auth::{ClientAuth, OAuth2ClientCredAuthPlugin};
use crate::infra::plugins::request_id_transform::RequestIdTransformPlugin;
use crate::infra::plugins::required_headers_guard::RequiredHeadersGuardPlugin;

/// The three plugin registries.
pub struct PluginRegistry {
    auth: HashMap<String, Arc<dyn AuthPlugin>>,
    guard: HashMap<String, Arc<dyn GuardPlugin>>,
    transform: HashMap<String, Arc<dyn TransformPlugin>>,
}

impl std::fmt::Debug for PluginRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PluginRegistry")
            .field("auth", &self.auth.keys().collect::<Vec<_>>())
            .field("guard", &self.guard.keys().collect::<Vec<_>>())
            .field("transform", &self.transform.keys().collect::<Vec<_>>())
            .finish()
    }
}

impl PluginRegistry {
    /// Registers the six built-in plugins (`DESIGN.md` § "Plugin Registry").
    #[must_use]
    pub fn with_builtins(cache_ttl: Duration, cache_capacity: usize) -> Self {
        let mut auth: HashMap<String, Arc<dyn AuthPlugin>> = HashMap::new();
        auth.insert(
            crate::domain::gts_helpers::AUTH_NOOP.to_owned(),
            Arc::new(NoopAuthPlugin),
        );
        auth.insert(
            crate::domain::gts_helpers::AUTH_APIKEY.to_owned(),
            Arc::new(ApiKeyAuthPlugin),
        );
        let form = OAuth2ClientCredAuthPlugin::new(
            crate::domain::gts_helpers::AUTH_OAUTH2,
            ClientAuth::Form,
            cache_ttl,
            cache_capacity,
        );
        let basic = OAuth2ClientCredAuthPlugin::new(
            crate::domain::gts_helpers::AUTH_OAUTH2_BASIC,
            ClientAuth::Basic,
            cache_ttl,
            cache_capacity,
        );
        auth.insert(form.id().to_owned(), Arc::new(form));
        auth.insert(basic.id().to_owned(), Arc::new(basic));

        let mut guard: HashMap<String, Arc<dyn GuardPlugin>> = HashMap::new();
        guard.insert(
            crate::domain::gts_helpers::GUARD_REQUIRED_HEADERS.to_owned(),
            Arc::new(RequiredHeadersGuardPlugin),
        );

        let mut transform: HashMap<String, Arc<dyn TransformPlugin>> = HashMap::new();
        transform.insert(
            crate::domain::gts_helpers::TRANSFORM_REQUEST_ID.to_owned(),
            Arc::new(RequestIdTransformPlugin),
        );

        Self {
            auth,
            guard,
            transform,
        }
    }

    /// Resolves an auth plugin.
    ///
    /// # Errors
    /// [`DomainError::PluginNotFound`] when the identifier has no
    /// implementation.
    pub fn auth(&self, id: &str) -> Result<Arc<dyn AuthPlugin>, DomainError> {
        self.auth
            .get(id)
            .cloned()
            .ok_or_else(|| DomainError::PluginNotFound(id.to_owned()))
    }

    /// Resolves a guard plugin.
    ///
    /// # Errors
    /// [`DomainError::PluginNotFound`] when the identifier has no
    /// implementation.
    pub fn guard(&self, id: &str) -> Result<Arc<dyn GuardPlugin>, DomainError> {
        self.guard
            .get(id)
            .cloned()
            .ok_or_else(|| DomainError::PluginNotFound(id.to_owned()))
    }

    /// Resolves a transform plugin.
    ///
    /// # Errors
    /// [`DomainError::PluginNotFound`] when the identifier has no
    /// implementation.
    pub fn transform(&self, id: &str) -> Result<Arc<dyn TransformPlugin>, DomainError> {
        self.transform
            .get(id)
            .cloned()
            .ok_or_else(|| DomainError::PluginNotFound(id.to_owned()))
    }

    /// Number of registered implementations across the three families.
    #[must_use]
    pub fn len(&self) -> usize {
        self.auth.len() + self.guard.len() + self.transform.len()
    }

    /// `true` when nothing is registered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_builtins_are_exactly_the_six_implemented_identifiers() {
        let registry = PluginRegistry::with_builtins(Duration::from_secs(1), 8);
        assert_eq!(registry.len(), 6);
        for id in crate::domain::type_catalog::bindable_ids() {
            match crate::domain::type_catalog::lookup(&id)
                .expect("catalog")
                .kind
            {
                crate::domain::model::PluginKind::Auth => {
                    assert!(registry.auth(&id).is_ok(), "{id}");
                }
                crate::domain::model::PluginKind::Guard => {
                    assert!(registry.guard(&id).is_ok(), "{id}");
                }
                crate::domain::model::PluginKind::Transform => {
                    assert!(registry.transform(&id).is_ok(), "{id}");
                }
            }
        }
    }

    #[test]
    fn catalog_only_identifiers_do_not_resolve() {
        let registry = PluginRegistry::with_builtins(Duration::from_secs(1), 8);
        assert!(matches!(
            registry.auth("gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.basic.v1"),
            Err(DomainError::PluginNotFound(_))
        ));
        assert!(matches!(
            registry.guard("gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.timeout.v1"),
            Err(DomainError::PluginNotFound(_))
        ));
        assert!(matches!(
            registry.transform("gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.metrics.v1"),
            Err(DomainError::PluginNotFound(_))
        ));
    }
}
