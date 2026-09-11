// Updated: 2026-09-01 by Constructor Tech
//! Plugin registry (ADR-0002 `AuthPluginRegistry`, `GuardPluginRegistry`,
//! `TransformPluginRegistry`).
//!
//! Three registries, one struct. Each is keyed by every spelling an operator
//! may legitimately use: the full GTS identifier, the bare type suffix
//! (`cf.core.oagw.apikey.v1`), and the short name (`apikey`). Resolution is
//! name-only — the runtime configuration of a binding travels separately in
//! the request context.
//!
//! Some identifiers exist only in the catalog (PRD marks them "catalog
//! identifier only"). They are known so that a binding referring to one is
//! rejected with a precise message rather than treated as a typo, but no
//! implementation is registered for them, so resolving one fails.

use std::collections::HashMap;
use std::sync::Arc;

use credstore_sdk::CredStoreClientV1;

use crate::config::TokenCacheConfig;
use crate::domain::plugin::{AuthPlugin, GuardPlugin, TransformPlugin};

/// Why a plugin reference could not be resolved.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PluginLookupError {
    /// The identifier is not in the catalog at all.
    #[error("unknown plugin '{0}'")]
    Unknown(String),
    /// The identifier is in the catalog but has no implementation.
    #[error("plugin '{0}' is catalogued but has no implementation and cannot be bound")]
    NotImplemented(String),
}

/// Registry of the plugins this process can execute.
#[derive(Default)]
pub struct PluginRegistry {
    auth: HashMap<String, Arc<dyn AuthPlugin>>,
    guard: HashMap<String, Arc<dyn GuardPlugin>>,
    transform: HashMap<String, Arc<dyn TransformPlugin>>,
    /// Identifiers reserved in the catalog. Every key of the three registries
    /// is in here too; the extra entries are the catalog-only ones.
    catalog: HashMap<String, &'static str>,
}

impl std::fmt::Debug for PluginRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PluginRegistry")
            .field("auth", &self.auth.keys().collect::<Vec<_>>())
            .field("guard", &self.guard.keys().collect::<Vec<_>>())
            .field("transform", &self.transform.keys().collect::<Vec<_>>())
            .finish_non_exhaustive()
    }
}

impl PluginRegistry {
    /// The registry with every builtin plugin registered.
    ///
    /// `credstore` backs the plugins that resolve a `secret_ref`; when it is
    /// absent those plugins are still registered and report an infrastructure
    /// failure at request time rather than silently disappearing.
    #[must_use]
    pub fn with_builtins(
        credstore: Option<Arc<dyn CredStoreClientV1>>,
        token_cache: TokenCacheConfig,
    ) -> Arc<Self> {
        let mut registry = Self::default();
        registry.register_auth(Arc::new(crate::infra::plugin::noop_auth::NoopAuthPlugin));
        registry.register_auth(Arc::new(
            crate::infra::plugin::apikey_auth::ApiKeyAuthPlugin::new(credstore.clone()),
        ));
        registry.register_auth(Arc::new(
            crate::infra::plugin::oauth2_client_cred_auth::OAuth2ClientCredAuthPlugin::new(
                credstore.clone(),
                toolkit_auth::oauth2::ClientAuthMethod::Form,
                token_cache.clone(),
            ),
        ));
        registry.register_auth(Arc::new(
            crate::infra::plugin::oauth2_client_cred_auth::OAuth2ClientCredAuthPlugin::new(
                credstore,
                toolkit_auth::oauth2::ClientAuthMethod::Basic,
                token_cache.clone(),
            ),
        ));
        registry.register_guard(Arc::new(
            crate::infra::plugin::required_headers_guard::RequiredHeadersGuardPlugin,
        ));
        registry.register_transform(Arc::new(
            crate::infra::plugin::request_id_transform::RequestIdTransformPlugin,
        ));
        registry.catalog_reserved(&[
            (crate::gts::AUTH_BASIC, "basic"),
            (crate::gts::AUTH_BEARER, "bearer"),
            (crate::gts::GUARD_TIMEOUT, "timeout"),
            (crate::gts::GUARD_CORS, "cors"),
            (crate::gts::TRANSFORM_LOGGING, "logging"),
            (crate::gts::TRANSFORM_METRICS, "metrics"),
        ]);
        Arc::new(registry)
    }

    /// A registry with nothing in it. Tests use this to check the failure
    /// paths; production always uses [`PluginRegistry::with_builtins`].
    #[must_use]
    pub fn empty() -> Arc<Self> {
        Arc::new(Self::default())
    }

    fn register_auth(&mut self, plugin: Arc<dyn AuthPlugin>) {
        for key in keys_for(plugin.plugin_type()) {
            self.auth.insert(key.clone(), Arc::clone(&plugin));
            self.catalog.insert(key, plugin.plugin_type());
        }
    }

    fn catalog_reserved(&mut self, entries: &[(&'static str, &'static str)]) {
        for (gts, short) in entries {
            for key in keys_for(gts) {
                self.catalog.insert(key, gts);
            }
            self.catalog.insert(short.to_string(), gts);
        }
    }

    /// Register an additional guard plugin at runtime (custom plugins).
    pub fn register_guard(&mut self, plugin: Arc<dyn GuardPlugin>) {
        for key in keys_for(plugin.plugin_type()) {
            self.guard.insert(key.clone(), Arc::clone(&plugin));
            self.catalog.insert(key, plugin.plugin_type());
        }
    }

    /// Register an additional transform plugin at runtime.
    pub fn register_transform(&mut self, plugin: Arc<dyn TransformPlugin>) {
        for key in keys_for(plugin.plugin_type()) {
            self.transform.insert(key.clone(), Arc::clone(&plugin));
            self.catalog.insert(key, plugin.plugin_type());
        }
    }

    /// Resolve an auth plugin by GTS identifier, type suffix or short name.
    ///
    /// # Errors
    ///
    /// [`PluginLookupError`] when the identifier is unknown or catalog-only.
    pub fn resolve_auth(&self, name: &str) -> Result<Arc<dyn AuthPlugin>, PluginLookupError> {
        self.resolve(&self.auth, name)
    }

    /// Resolve a guard plugin.
    ///
    /// # Errors
    ///
    /// [`PluginLookupError`] when the identifier is unknown or catalog-only.
    pub fn resolve_guard(&self, name: &str) -> Result<Arc<dyn GuardPlugin>, PluginLookupError> {
        self.resolve(&self.guard, name)
    }

    /// Resolve a transform plugin.
    ///
    /// # Errors
    ///
    /// [`PluginLookupError`] when the identifier is unknown or catalog-only.
    pub fn resolve_transform(
        &self,
        name: &str,
    ) -> Result<Arc<dyn TransformPlugin>, PluginLookupError> {
        self.resolve(&self.transform, name)
    }

    fn resolve<T: ?Sized + 'static>(
        &self,
        table: &HashMap<String, Arc<T>>,
        name: &str,
    ) -> Result<Arc<T>, PluginLookupError> {
        for key in keys_for(name) {
            if let Some(found) = table.get(&key) {
                return Ok(found.clone());
            }
        }
        // Distinguish "never heard of it" from "reserved but not implemented".
        for key in keys_for(name) {
            if self.catalog.contains_key(&key) {
                return Err(PluginLookupError::NotImplemented(name.to_owned()));
            }
        }
        Err(PluginLookupError::Unknown(name.to_owned()))
    }

    /// Whether the identifier is known at all — implemented or catalog-only.
    #[must_use]
    pub fn is_builtin(&self, name: &str) -> bool {
        keys_for(name).iter().any(|k| self.catalog.contains_key(k))
    }

    /// Whether the identifier is known *and* executable.
    #[must_use]
    pub fn is_implemented(&self, name: &str) -> bool {
        keys_for(name).iter().any(|k| {
            self.auth.contains_key(k)
                || self.guard.contains_key(k)
                || self.transform.contains_key(k)
        })
    }
}

/// Every spelling under which a plugin type may be referenced.
fn keys_for(name: &str) -> Vec<String> {
    let raw = name.trim();
    // Strip the GTS base type from an instance identifier
    // (`gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1`).
    let tail = raw.rsplit('~').next().unwrap_or(raw);
    let mut keys = vec![raw.to_owned()];
    if tail != raw {
        keys.push(tail.to_owned());
    }
    // A dotted instance name ends in a version, so its trailing label is
    // `v1` and carries nothing; the label before it is the distinguishing
    // short name. A name without dots — a UUID-backed custom reference —
    // has no short form.
    let labels: Vec<&str> = tail.split('.').collect();
    if labels.len() >= 2 {
        let short = labels[labels.len() - 2];
        if !keys.iter().any(|k| k == short) {
            keys.push(short.to_owned());
        }
    }
    keys
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_resolves_by_every_spelling() {
        let registry = PluginRegistry::with_builtins(None, TokenCacheConfig::default());
        for spelling in [crate::gts::AUTH_APIKEY, "cf.core.oagw.apikey.v1", "apikey"] {
            assert!(registry.resolve_auth(spelling).is_ok(), "{spelling}");
        }
    }

    #[test]
    fn catalog_only_auth_is_rejected_with_a_precise_error() {
        let registry = PluginRegistry::with_builtins(None, TokenCacheConfig::default());
        for spelling in [crate::gts::AUTH_BASIC, "basic", crate::gts::AUTH_BEARER] {
            let err = registry.resolve_auth(spelling).unwrap_err();
            assert!(
                matches!(err, PluginLookupError::NotImplemented(_)),
                "{spelling} -> {err}"
            );
        }
    }

    #[test]
    fn unknown_plugin_is_unknown() {
        let registry = PluginRegistry::with_builtins(None, TokenCacheConfig::default());
        let err = registry.resolve_auth("gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.nope.v1");
        assert!(matches!(err, Err(PluginLookupError::Unknown(_))));
    }

    #[test]
    fn catalog_only_guard_and_transform_are_not_bindable() {
        let registry = PluginRegistry::with_builtins(None, TokenCacheConfig::default());
        assert!(registry.resolve_guard(crate::gts::GUARD_TIMEOUT).is_err());
        assert!(registry.resolve_guard(crate::gts::GUARD_CORS).is_err());
        assert!(
            registry
                .resolve_transform(crate::gts::TRANSFORM_LOGGING)
                .is_err()
        );
        assert!(
            registry
                .resolve_transform(crate::gts::TRANSFORM_METRICS)
                .is_err()
        );
    }

    #[test]
    fn the_bindable_builtins_resolve() {
        let registry = PluginRegistry::with_builtins(None, TokenCacheConfig::default());
        assert!(
            registry
                .resolve_guard(crate::gts::GUARD_REQUIRED_HEADERS)
                .is_ok()
        );
        assert!(
            registry
                .resolve_transform(crate::gts::TRANSFORM_REQUEST_ID)
                .is_ok()
        );
        assert!(registry.resolve_auth(crate::gts::AUTH_NOOP).is_ok());
        assert!(registry.resolve_auth(crate::gts::AUTH_OAUTH2_CC).is_ok());
        assert!(
            registry
                .resolve_auth(crate::gts::AUTH_OAUTH2_CC_BASIC)
                .is_ok()
        );
    }

    #[test]
    fn is_builtin_covers_the_whole_catalog() {
        let registry = PluginRegistry::with_builtins(None, TokenCacheConfig::default());
        assert!(registry.is_builtin(crate::gts::TRANSFORM_METRICS));
        assert!(!registry.is_builtin("gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.zzz.v1"));
    }
}
