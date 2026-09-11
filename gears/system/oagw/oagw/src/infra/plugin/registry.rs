//! Plugin registries.
//!
//! A registry maps a catalog identifier to a *factory* that builds a plugin
//! instance from a binding's configuration, so built-in plugins stay
//! stateless and route bindings stay per-route. ADR 0002 fixes the plugin
//! traits; this module fixes how they are discovered and instantiated.

use std::collections::BTreeMap;
use std::sync::Arc;

use parking_lot::RwLock;

use crate::domain::error::DomainError;
use crate::domain::plugin::{
    AuthPlugin, GuardPlugin, PluginCatalog, PluginDescriptor, PluginType, TransformPlugin,
};

/// A catalog entry: what an operator can bind, and whether it has an
/// implementation in this build.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginCatalogEntry {
    /// Plugin identifier.
    pub id: String,
    /// Plugin class.
    pub plugin_type: PluginType,
    /// Implementation version.
    pub version: String,
    /// Operator-facing description.
    pub description: String,
    /// Whether this build ships an implementation.
    pub built_in: bool,
}

/// Builds a plugin instance from a binding's configuration.
///
/// `P` is the plugin class as a trait object (`dyn AuthPlugin`), so the
/// parameter is explicitly allowed to be unsized.
pub trait PluginFactory<P: ?Sized>: Send + Sync {
    /// Catalog identifier the factory answers to.
    fn id(&self) -> &str;

    /// Class of plugin the factory builds.
    fn plugin_type(&self) -> PluginType;

    /// What the catalog tells an operator this plugin does.
    fn description(&self) -> &'static str {
        ""
    }

    /// Build an instance for `config`.
    ///
    /// # Errors
    /// Returns an error when the configuration is not valid for this plugin.
    fn create(
        &self,
        config: &serde_json::Map<String, serde_json::Value>,
    ) -> Result<Arc<P>, DomainError>;
}

/// Shared factory handle.
type Factory<P> = Arc<dyn PluginFactory<P>>;

struct RegistryState<P: ?Sized> {
    factories: BTreeMap<String, Factory<P>>,
    /// Identifiers known to the catalog but with no implementation.
    catalog_only: BTreeMap<String, PluginCatalogEntry>,
}

// Written by hand so the bound is `P: 'static`, not `P: Default`: a registry
// is keyed by plugin *identifier*, so the plugin class itself never needs a
// default value.
impl<P: ?Sized + 'static> Default for RegistryState<P> {
    fn default() -> Self {
        Self {
            factories: BTreeMap::new(),
            catalog_only: BTreeMap::new(),
        }
    }
}

/// A registry of plugin factories for one plugin class.
pub struct PluginRegistry<P: ?Sized> {
    state: RwLock<RegistryState<P>>,
    class: PluginType,
}

impl<P: ?Sized + 'static> PluginRegistry<P> {
    /// Create an empty registry for `class`.
    #[must_use]
    pub fn new(class: PluginType) -> Self {
        Self {
            state: RwLock::new(RegistryState::default()),
            class,
        }
    }

    /// Register a factory.
    pub fn register(&self, factory: Factory<P>) {
        let id = factory.id().to_owned();
        self.state.write().factories.insert(id, factory);
    }

    /// Build an instance for the binding `(id, config)`.
    ///
    /// # Errors
    /// Returns [`ErrorKind::Validation`] when the identifier is not in the
    /// catalog at all, and [`ErrorKind::PluginNotFound`] when it is known but
    /// this build ships no implementation for it.
    pub fn create(
        &self,
        id: &str,
        config: &serde_json::Map<String, serde_json::Value>,
    ) -> Result<Arc<P>, DomainError> {
        let state = self.state.read();
        let Some(factory) = state.factories.get(id) else {
            return Err(if state.catalog_only.contains_key(id) {
                DomainError::plugin_not_found(format!(
                    "plugin '{id}' is known to the catalog but has no implementation"
                ))
            } else {
                DomainError::validation(format!("unknown plugin '{id}'"))
            });
        };
        factory.create(config)
    }

    /// Whether `id` has an implementation.
    #[must_use]
    pub fn has(&self, id: &str) -> bool {
        self.state.read().factories.contains_key(id)
    }

    /// Identifiers with implementations.
    #[must_use]
    pub fn ids(&self) -> Vec<String> {
        self.state.read().factories.keys().cloned().collect()
    }

    /// Identifiers known to the catalog but without an implementation.
    #[must_use]
    pub fn catalog_only_ids(&self) -> Vec<String> {
        self.state.read().catalog_only.keys().cloned().collect()
    }

    /// Every catalog entry, built-ins first by identifier.
    #[must_use]
    pub fn catalog(&self) -> Vec<PluginCatalogEntry> {
        let state = self.state.read();
        let mut out: Vec<PluginCatalogEntry> = state
            .factories
            .iter()
            .map(|(id, factory)| PluginCatalogEntry {
                id: id.clone(),
                plugin_type: factory.plugin_type(),
                version: "1".to_owned(),
                description: factory.description().to_owned(),
                built_in: true,
            })
            .collect();
        out.extend(state.catalog_only.values().cloned());
        out
    }

    /// Whether this registry is for `class`.
    #[must_use]
    pub const fn class(&self) -> PluginType {
        self.class
    }

    /// Declare a catalog-only identifier: known, but with no implementation.
    pub fn declare_catalog_only(&self, entry: PluginCatalogEntry) {
        self.state
            .write()
            .catalog_only
            .insert(entry.id.clone(), entry);
    }
}

/// Registry of auth plugins.
#[derive(Default)]
pub struct AuthPluginRegistry {
    inner: PluginRegistry<dyn AuthPlugin>,
}

impl std::fmt::Debug for AuthPluginRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuthPluginRegistry")
            .field("plugins", &self.inner.ids())
            .finish()
    }
}

impl Default for PluginRegistry<dyn AuthPlugin> {
    fn default() -> Self {
        Self::new(PluginType::Auth)
    }
}

impl std::ops::Deref for AuthPluginRegistry {
    type Target = PluginRegistry<dyn AuthPlugin>;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl AuthPluginRegistry {
    /// Create an empty auth registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The auth plugins this build ships: `noop` and `apikey`.
    #[must_use]
    pub fn with_builtins() -> Self {
        let registry = Self::new();
        registry.register(Arc::new(super::noop_auth::NoopAuthFactory));
        registry.register(Arc::new(super::apikey_auth::ApiKeyAuthFactory));
        // Known to the catalog, but with no implementation in this build.
        for (id, description) in [
            ("basic", "HTTP Basic authentication against the upstream"),
            ("bearer", "Static bearer-token injection from a secret_ref"),
            ("oauth2", "OAuth2 client-credentials token acquisition"),
        ] {
            registry.declare_catalog_only(PluginCatalogEntry {
                id: id.to_owned(),
                plugin_type: PluginType::Auth,
                version: "1".to_owned(),
                description: description.to_owned(),
                built_in: false,
            });
        }
        registry
    }
}

/// Registry of guard plugins.
#[derive(Default)]
pub struct GuardPluginRegistry {
    inner: PluginRegistry<dyn GuardPlugin>,
}

impl std::fmt::Debug for GuardPluginRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GuardPluginRegistry")
            .field("plugins", &self.inner.ids())
            .finish()
    }
}

impl Default for PluginRegistry<dyn GuardPlugin> {
    fn default() -> Self {
        Self::new(PluginType::Guard)
    }
}

impl std::ops::Deref for GuardPluginRegistry {
    type Target = PluginRegistry<dyn GuardPlugin>;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl GuardPluginRegistry {
    /// Create an empty guard registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The guard plugins this build ships: `required_headers` only (ADR 0009).
    #[must_use]
    pub fn with_builtins() -> Self {
        let registry = Self::new();
        registry.register(Arc::new(
            super::required_headers_guard::RequiredHeadersGuardFactory,
        ));
        for (id, description) in [
            (
                "timeout",
                "Per-request duration budget enforced at the gateway",
            ),
            ("cors", "Cross-origin policy for a single route"),
        ] {
            registry.declare_catalog_only(PluginCatalogEntry {
                id: id.to_owned(),
                plugin_type: PluginType::Guard,
                version: "1".to_owned(),
                description: description.to_owned(),
                built_in: false,
            });
        }
        registry
    }
}

/// Registry of transform plugins.
#[derive(Default)]
pub struct TransformPluginRegistry {
    inner: PluginRegistry<dyn TransformPlugin>,
}

impl std::fmt::Debug for TransformPluginRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TransformPluginRegistry")
            .field("plugins", &self.inner.ids())
            .finish()
    }
}

impl Default for PluginRegistry<dyn TransformPlugin> {
    fn default() -> Self {
        Self::new(PluginType::Transform)
    }
}

impl std::ops::Deref for TransformPluginRegistry {
    type Target = PluginRegistry<dyn TransformPlugin>;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl TransformPluginRegistry {
    /// Create an empty transform registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The transform plugins this build ships: `request_id`.
    #[must_use]
    pub fn with_builtins() -> Self {
        let registry = Self::new();
        registry.register(Arc::new(
            super::request_id_transform::RequestIdTransformFactory,
        ));
        for (id, description) in [
            ("logging", "Structured access logging for one route"),
            ("metrics", "Per-route request and latency metrics"),
        ] {
            registry.declare_catalog_only(PluginCatalogEntry {
                id: id.to_owned(),
                plugin_type: PluginType::Transform,
                version: "1".to_owned(),
                description: description.to_owned(),
                built_in: false,
            });
        }
        registry
    }
}

impl PluginCatalog for AuthPluginRegistry {
    fn descriptors(&self) -> Vec<PluginDescriptor> {
        self.catalog()
            .into_iter()
            .map(|entry| PluginDescriptor {
                id: entry.id,
                plugin_type: entry.plugin_type,
                version: entry.version,
                description: entry.description,
                built_in: entry.built_in,
            })
            .collect()
    }

    fn descriptor(&self, id: &str) -> Option<PluginDescriptor> {
        self.catalog()
            .into_iter()
            .find(|entry| entry.id == id)
            .map(|entry| PluginDescriptor {
                id: entry.id,
                plugin_type: entry.plugin_type,
                version: entry.version,
                description: entry.description,
                built_in: entry.built_in,
            })
    }
}

impl PluginCatalog for GuardPluginRegistry {
    fn descriptors(&self) -> Vec<PluginDescriptor> {
        self.catalog()
            .into_iter()
            .map(|entry| PluginDescriptor {
                id: entry.id,
                plugin_type: entry.plugin_type,
                version: entry.version,
                description: entry.description,
                built_in: entry.built_in,
            })
            .collect()
    }

    fn descriptor(&self, id: &str) -> Option<PluginDescriptor> {
        self.catalog()
            .into_iter()
            .find(|entry| entry.id == id)
            .map(|entry| PluginDescriptor {
                id: entry.id,
                plugin_type: entry.plugin_type,
                version: entry.version,
                description: entry.description,
                built_in: entry.built_in,
            })
    }
}

impl PluginCatalog for TransformPluginRegistry {
    fn descriptors(&self) -> Vec<PluginDescriptor> {
        self.catalog()
            .into_iter()
            .map(|entry| PluginDescriptor {
                id: entry.id,
                plugin_type: entry.plugin_type,
                version: entry.version,
                description: entry.description,
                built_in: entry.built_in,
            })
            .collect()
    }

    fn descriptor(&self, id: &str) -> Option<PluginDescriptor> {
        self.catalog()
            .into_iter()
            .find(|entry| entry.id == id)
            .map(|entry| PluginDescriptor {
                id: entry.id,
                plugin_type: entry.plugin_type,
                version: entry.version,
                description: entry.description,
                built_in: entry.built_in,
            })
    }
}

/// The combined catalog: every registry's entries, in class order.
#[derive(Debug, Default)]
pub struct PluginCatalogView {
    auth: Arc<AuthPluginRegistry>,
    guard: Arc<GuardPluginRegistry>,
    transform: Arc<TransformPluginRegistry>,
}

impl PluginCatalogView {
    /// Assemble the view over the three registries.
    #[must_use]
    pub const fn new(
        auth: Arc<AuthPluginRegistry>,
        guard: Arc<GuardPluginRegistry>,
        transform: Arc<TransformPluginRegistry>,
    ) -> Self {
        Self {
            auth,
            guard,
            transform,
        }
    }
}

impl PluginCatalog for PluginCatalogView {
    fn descriptors(&self) -> Vec<PluginDescriptor> {
        let mut out = self.auth.descriptors();
        out.extend(self.guard.descriptors());
        out.extend(self.transform.descriptors());
        out
    }

    fn descriptor(&self, id: &str) -> Option<PluginDescriptor> {
        self.auth
            .descriptor(id)
            .or_else(|| self.guard.descriptor(id))
            .or_else(|| self.transform.descriptor(id))
    }
}

#[cfg(test)]
mod registry_tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use crate::domain::error::ErrorKind;
    use crate::domain::plugin::{ErrorContext, RequestContext, ResponseContext};
    use async_trait::async_trait;

    /// The error `registry.create` produces for `id`; `unwrap_err` needs
    /// `Debug` on the plugin, which the trait object does not carry.
    fn create_err(
        registry: &AuthPluginRegistry,
        id: &str,
        cfg: &serde_json::Map<String, serde_json::Value>,
    ) -> DomainError {
        match registry.create(id, cfg) {
            Ok(_) => panic!("creating '{id}' was expected to fail"),
            Err(err) => err,
        }
    }

    /// A factory for a test auth plugin, echoing its config into a header.
    struct TestAuthFactory {
        id: &'static str,
    }

    #[async_trait]
    impl AuthPlugin for TestAuth {
        fn id(&self) -> &'static str {
            self.id
        }
        fn plugin_type(&self) -> &'static str {
            "auth"
        }
        async fn authenticate(&self, ctx: &mut RequestContext) -> Result<(), DomainError> {
            ctx.set_header("x-test", &self.value);
            Ok(())
        }
    }

    #[derive(Debug)]
    struct TestAuth {
        id: &'static str,
        value: String,
    }

    impl PluginFactory<dyn AuthPlugin> for TestAuthFactory {
        fn id(&self) -> &str {
            self.id
        }
        fn plugin_type(&self) -> PluginType {
            PluginType::Auth
        }
        fn create(
            &self,
            config: &serde_json::Map<String, serde_json::Value>,
        ) -> Result<Arc<dyn AuthPlugin>, DomainError> {
            let value = config
                .get("value")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_owned();
            Ok(Arc::new(TestAuth { id: self.id, value }))
        }
    }

    fn config(pairs: &[(&str, &str)]) -> serde_json::Map<String, serde_json::Value> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), serde_json::Value::from(*v)))
            .collect()
    }

    #[test]
    fn registry_builds_instances_from_config() {
        let registry = AuthPluginRegistry::new();
        registry.register(Arc::new(TestAuthFactory { id: "test" }));
        assert!(registry.has("test"));
        assert_eq!(registry.ids(), vec!["test".to_owned()]);

        let plugin = registry
            .create("test", &config(&[("value", "abc")]))
            .unwrap();
        assert_eq!(plugin.id(), "test");
    }

    #[test]
    fn unknown_identifier_is_a_validation_error() {
        let registry = AuthPluginRegistry::new();
        let err = create_err(&registry, "nope", &config(&[]));
        assert_eq!(err.kind(), ErrorKind::Validation);
    }

    #[test]
    fn catalog_only_identifier_is_plugin_not_found() {
        let registry = AuthPluginRegistry::new();
        registry.declare_catalog_only(PluginCatalogEntry {
            id: "basic".to_owned(),
            plugin_type: PluginType::Auth,
            version: "1".to_owned(),
            description: "HTTP Basic".to_owned(),
            built_in: false,
        });
        let err = create_err(&registry, "basic", &config(&[]));
        assert_eq!(err.kind(), ErrorKind::PluginNotFound);
        assert_eq!(registry.catalog_only_ids(), vec!["basic".to_owned()]);
        let catalog = registry.catalog();
        assert!(catalog.iter().any(|e| e.id == "basic" && !e.built_in));
    }

    #[test]
    fn registries_are_class_scoped() {
        let auth = AuthPluginRegistry::new();
        let guard = GuardPluginRegistry::new();
        let transform = TransformPluginRegistry::new();
        assert_eq!(auth.class(), PluginType::Auth);
        assert_eq!(guard.class(), PluginType::Guard);
        assert_eq!(transform.class(), PluginType::Transform);
        assert!(guard.catalog().is_empty());
        assert!(transform.catalog().is_empty());
    }

    // Silence unused-import warnings for the trait imports used by the
    // doc-level contract above.
    #[allow(unused)]
    fn _context_shapes(_r: &RequestContext, _s: &ResponseContext, _e: &ErrorContext) {}
}
