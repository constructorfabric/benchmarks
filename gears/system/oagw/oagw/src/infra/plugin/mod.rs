//! Built-in plugins and the plugin registry (ADR-0002 "Built-in Plugins").
//!
//! Every built-in is a native Rust implementation registered under its GTS
//! plugin id; external plugins (separate ToolKit gears) register through the
//! same [`PluginRegistry`] factories.
//!
//! | GTS plugin id | Kind | Module |
//! |---|---|---|
//! | `...auth_plugin.v1~cf.core.oagw.noop.v1` | auth | [`noop`] |
//! | `...auth_plugin.v1~cf.core.oagw.apikey.v1` | auth | [`apikey`] |
//! | `...auth_plugin.v1~cf.core.oagw.oauth2_client_cred.v1` | auth | [`oauth2`] |
//! | `...auth_plugin.v1~cf.core.oagw.oauth2_client_cred_basic.v1` | auth | [`oauth2`] |
//! | `...guard_plugin.v1~cf.core.oagw.required_headers.v1` | guard | [`required_headers`] |
//! | `...transform_plugin.v1~cf.core.oagw.request_id.v1` | transform | [`request_id`] |
//!
//! The catalog-only identifiers (`basic`, `bearer`, `timeout`, `cors`,
//! `logging`, `metrics`) are deliberately **not** registered: they have no
//! backing implementation or are core data-plane logic, so binding one must
//! fail with `503 PluginNotFound` instead of silently doing nothing.
//!
//! The same loud failure is the fate of an *enabled* plugin resource the
//! process cannot run (a custom plugin registered over `POST /plugins`, whose
//! Starlark runtime is not deployed here): [`PluginRegistry::build_chain`]
//! answers `503 PluginNotFound` rather than dropping a plugin an operator
//! asked for. A plugin resource with `enabled: false` is the opposite case: its
//! binding is **skipped**, because an operator disabling a plugin expects the
//! request to be served without it, not the upstream to break. The
//! `upstream.auth` binding is never skipped. The disabled set is handed in by
//! the caller as a [`DisabledPlugins`], so the registry stays free of any
//! `RegistryStore` dependency.
//!
//! Configuration is parsed once per plugin construction and treated as
//! immutable (ADR-0002), which is why the guard phase — whose ADR-0002
//! signature takes `&RequestContext` — can read it directly.
//!
//! ## Instance memoisation
//!
//! A plugin instance is stateful by design: the ADR-0008 OAuth2 plugin owns the
//! token cache that keeps the IdP out of the per-request path. Because
//! [`PluginRegistry::build_chain`] runs for *every* proxied request, the
//! registry memoises the instances it constructed, keyed by the resolved
//! registry key and a fingerprint of the binding configuration, so a binding
//! always resolves to the same instance — and therefore to the same token
//! cache — for as long as the registry lives. A construction that *failed* is
//! never memoised, so a misconfigured binding keeps failing (and keeps being
//! retried) until its configuration changes.

pub mod apikey;
pub mod noop;
pub mod oauth2;
pub mod request_id;
pub mod required_headers;
pub mod secret;

use std::collections::HashMap;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::Arc;

use parking_lot::RwLock;

use crate::domain::error::OagwError;
use crate::domain::model::{Plugin, PluginBinding, Route, Upstream, reference_matches_plugin};
use crate::domain::plugin::{
    AUTH_PLUGIN_TYPE_ID, AuthPlugin, GUARD_PLUGIN_TYPE_ID, GuardPlugin, PluginChain, PluginTier,
    TRANSFORM_PLUGIN_TYPE_ID, TransformPlugin,
};
use crate::infra::plugin::apikey::ApiKeyAuthPlugin;
use crate::infra::plugin::noop::NoopAuthPlugin;
use crate::infra::plugin::oauth2::OAuth2ClientCredAuthPlugin;
use crate::infra::plugin::request_id::RequestIdTransformPlugin;
use crate::infra::plugin::required_headers::RequiredHeadersGuardPlugin;
use crate::infra::plugin::secret::SecretResolver;

pub use crate::infra::plugin::oauth2::{
    BASIC_AUTH_METHOD_TAG, FORM_AUTH_METHOD_TAG, TokenCacheConfig,
};
pub use crate::infra::plugin::request_id::REQUEST_ID_HEADER;
pub use crate::infra::plugin::required_headers::REQUIRED_HEADER_MISSING;
pub use crate::infra::plugin::secret::{
    CredStoreSecretResolver, SecretResolver as SecretResolverTrait, StaticSecretResolver,
    UnavailableSecretResolver, strip_secret_scheme,
};

/// Splits a comma-separated header list into the ADR-0009 normal form: entries
/// are trimmed, lower-cased and empty entries are dropped.
#[must_use]
pub fn parse_header_names(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
        .map(str::to_ascii_lowercase)
        .collect()
}

/// Factory of an auth plugin from a binding configuration payload.
pub type AuthPluginFactory =
    Arc<dyn Fn(&serde_json::Value) -> Result<Arc<dyn AuthPlugin>, OagwError> + Send + Sync>;

/// Factory of a guard plugin from a binding configuration payload.
pub type GuardPluginFactory =
    Arc<dyn Fn(&serde_json::Value) -> Result<Arc<dyn GuardPlugin>, OagwError> + Send + Sync>;

/// Factory of a transform plugin from a binding configuration payload.
pub type TransformPluginFactory =
    Arc<dyn Fn(&serde_json::Value) -> Result<Arc<dyn TransformPlugin>, OagwError> + Send + Sync>;

/// A plugin the registry resolved by id, whatever kind it is.
#[derive(Clone)]
enum ResolvedPlugin {
    Auth(Arc<dyn AuthPlugin>),
    Guard(Arc<dyn GuardPlugin>),
    Transform(Arc<dyn TransformPlugin>),
}

/// Memoisation key of a constructed plugin: the resolved registry key plus a
/// fingerprint of the binding configuration.
type ConstructionKey = (String, u64);

/// Upper bound of the memo table.
///
/// A gateway binds a handful of plugin configurations, so a thousand
/// constructions cover every realistic deployment. Reaching the bound clears
/// the table wholesale — the same flush-on-full policy the L1 caches use — so
/// a churn of configurations cannot grow it without bound.
const CONSTRUCTED_CAPACITY: usize = 1024;

/// The disabled plugin resources a chain build must skip.
///
/// The data-plane caller reads them out of its registry store (the disabled
/// plugins of the resolved tenant chain) and hands them to
/// [`PluginRegistry::build_chain`], which keeps [`PluginRegistry`] free of any
/// store dependency. A binding reference matches one of these plugins in either
/// wire spelling — the bare instance UUID and the GTS-form
/// `gts.cf.core.oagw.plugin.v1~{uuid}` id (see [`reference_matches_plugin`]) —
/// while a built-in plugin id never matches, because its instance part is not a
/// UUID.
#[derive(Debug, Clone, Default)]
pub struct DisabledPlugins {
    /// The disabled plugin resources of the resolved tenant chain.
    plugins: Vec<Arc<Plugin>>,
}

impl DisabledPlugins {
    /// Collects the disabled plugins (`enabled: false`) of `plugins`.
    #[must_use]
    pub fn of<I: IntoIterator<Item = Arc<Plugin>>>(plugins: I) -> Self {
        Self {
            plugins: plugins
                .into_iter()
                .filter(|plugin| !plugin.enabled)
                .collect(),
        }
    }

    /// `true` when no plugin resource is disabled.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.plugins.is_empty()
    }

    /// `true` when `reference` names a disabled plugin resource, in either wire
    /// spelling.
    #[must_use]
    pub fn is_disabled(&self, reference: &str) -> bool {
        self.plugins
            .iter()
            .any(|plugin| reference_matches_plugin(reference, plugin))
    }
}

/// Maps plugin ids to the factories that construct them.
///
/// Lookups accept the full GTS id and the bare instance fragment
/// (`cf.core.oagw.apikey.v1`), so an operator may write either form; a ref
/// that resolves to nothing is a `503` [`OagwError::PluginNotFound`].
#[derive(Default)]
pub struct PluginRegistry {
    auth: HashMap<String, AuthPluginFactory>,
    guard: HashMap<String, GuardPluginFactory>,
    transform: HashMap<String, TransformPluginFactory>,
    /// Plugin instances already constructed, by `(resolved key, config hash)`.
    constructed: RwLock<HashMap<ConstructionKey, ResolvedPlugin>>,
}

impl Clone for PluginRegistry {
    fn clone(&self) -> Self {
        Self {
            auth: self.auth.clone(),
            guard: self.guard.clone(),
            transform: self.transform.clone(),
            constructed: RwLock::new(self.constructed.read().clone()),
        }
    }
}

impl PluginRegistry {
    /// Creates an empty registry (no built-ins).
    #[must_use]
    pub fn empty() -> Self {
        Self::default()
    }

    /// Creates a registry with every built-in plugin (ADR-0002).
    ///
    /// `resolver` sources the `cred://` references of the auth plugins; pass
    /// [`UnavailableSecretResolver`] when the gear runs without a credential
    /// store, which fails every resolution with `500 SecretNotFound` at use
    /// time rather than degrading to an unauthenticated upstream call.
    #[must_use]
    pub fn with_builtins(resolver: Arc<dyn SecretResolver>, token_cache: TokenCacheConfig) -> Self {
        let mut registry = Self::empty();
        registry.register_auth(noop::NoopAuthPlugin::PLUGIN_ID, {
            Arc::new(move |_config| Ok(Arc::new(NoopAuthPlugin) as Arc<dyn AuthPlugin>))
        });
        registry.register_auth(apikey::ApiKeyAuthPlugin::PLUGIN_ID, {
            let resolver = Arc::clone(&resolver);
            Arc::new(move |config| {
                let resolver = Arc::clone(&resolver);
                let plugin = ApiKeyAuthPlugin::new(resolver, config)?;
                Ok(Arc::new(plugin) as Arc<dyn AuthPlugin>)
            })
        });
        registry.register_auth(oauth2::OAuth2ClientCredAuthPlugin::FORM_PLUGIN_ID, {
            let resolver = Arc::clone(&resolver);
            Arc::new(move |config| {
                let resolver = Arc::clone(&resolver);
                let plugin = OAuth2ClientCredAuthPlugin::new(
                    resolver,
                    toolkit_auth::ClientAuthMethod::Form,
                    token_cache,
                    config,
                )?;
                Ok(Arc::new(plugin) as Arc<dyn AuthPlugin>)
            })
        });
        registry.register_auth(oauth2::OAuth2ClientCredAuthPlugin::BASIC_PLUGIN_ID, {
            let resolver = Arc::clone(&resolver);
            Arc::new(move |config| {
                let resolver = Arc::clone(&resolver);
                let plugin = OAuth2ClientCredAuthPlugin::new(
                    resolver,
                    toolkit_auth::ClientAuthMethod::Basic,
                    token_cache,
                    config,
                )?;
                Ok(Arc::new(plugin) as Arc<dyn AuthPlugin>)
            })
        });
        registry.register_guard(
            required_headers::RequiredHeadersGuardPlugin::PLUGIN_ID,
            Arc::new(|config| {
                Ok(Arc::new(RequiredHeadersGuardPlugin::new(config)) as Arc<dyn GuardPlugin>)
            }),
        );
        registry.register_transform(
            request_id::RequestIdTransformPlugin::PLUGIN_ID,
            Arc::new(|_config| Ok(Arc::new(RequestIdTransformPlugin) as Arc<dyn TransformPlugin>)),
        );
        registry
    }

    /// Registers an auth plugin factory under `plugin_ref`.
    ///
    /// Re-registering a reference forgets the instances built for it: a new
    /// factory must never be shadowed by a memoised predecessor.
    pub fn register_auth(&mut self, plugin_ref: impl Into<String>, factory: AuthPluginFactory) {
        self.auth.insert(plugin_ref.into(), factory);
        self.constructed.write().clear();
    }

    /// Registers a guard plugin factory under `plugin_ref`.
    ///
    /// Re-registering a reference forgets the instances built for it, exactly
    /// like [`PluginRegistry::register_auth`].
    pub fn register_guard(&mut self, plugin_ref: impl Into<String>, factory: GuardPluginFactory) {
        self.guard.insert(plugin_ref.into(), factory);
        self.constructed.write().clear();
    }

    /// Registers a transform plugin factory under `plugin_ref`.
    ///
    /// Re-registering a reference forgets the instances built for it, exactly
    /// like [`PluginRegistry::register_auth`].
    pub fn register_transform(
        &mut self,
        plugin_ref: impl Into<String>,
        factory: TransformPluginFactory,
    ) {
        self.transform.insert(plugin_ref.into(), factory);
        self.constructed.write().clear();
    }

    /// `true` when `plugin_ref` resolves to a registered plugin.
    #[must_use]
    pub fn contains(&self, plugin_ref: &str) -> bool {
        self.resolve_key(plugin_ref).is_some()
    }

    /// Number of registered plugins, across all three kinds.
    #[must_use]
    pub fn len(&self) -> usize {
        self.auth.len() + self.guard.len() + self.transform.len()
    }

    /// `true` when no plugin is registered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Builds an auth plugin from `(id, config)`.
    ///
    /// # Errors
    ///
    /// Returns [`OagwError::PluginNotFound`] when the id is unknown, and the
    /// plugin's own error when the configuration is invalid.
    pub fn build_auth(
        &self,
        plugin_ref: &str,
        config: &serde_json::Value,
    ) -> Result<Arc<dyn AuthPlugin>, OagwError> {
        match self.build_any(plugin_ref, config)? {
            ResolvedPlugin::Auth(plugin) => Ok(plugin),
            _ => Err(plugin_not_found(plugin_ref)),
        }
    }

    /// Builds a guard plugin from `(id, config)`.
    ///
    /// # Errors
    ///
    /// Returns [`OagwError::PluginNotFound`] when the id is unknown, and the
    /// plugin's own error when the configuration is invalid.
    pub fn build_guard(
        &self,
        plugin_ref: &str,
        config: &serde_json::Value,
    ) -> Result<Arc<dyn GuardPlugin>, OagwError> {
        match self.build_any(plugin_ref, config)? {
            ResolvedPlugin::Guard(plugin) => Ok(plugin),
            _ => Err(plugin_not_found(plugin_ref)),
        }
    }

    /// Builds a transform plugin from `(id, config)`.
    ///
    /// # Errors
    ///
    /// Returns [`OagwError::PluginNotFound`] when the id is unknown, and the
    /// plugin's own error when the configuration is invalid.
    pub fn build_transform(
        &self,
        plugin_ref: &str,
        config: &serde_json::Value,
    ) -> Result<Arc<dyn TransformPlugin>, OagwError> {
        match self.build_any(plugin_ref, config)? {
            ResolvedPlugin::Transform(plugin) => Ok(plugin),
            _ => Err(plugin_not_found(plugin_ref)),
        }
    }

    /// Builds the ADR-0002 plugin chain of one proxy request: the upstream
    /// auth binding first, then the upstream plugin chain, then the route
    /// plugin chain (upstream tier before route tier, declaration order kept).
    ///
    /// A plugin binding whose reference names one of `disabled_plugins` (a
    /// plugin resource with `enabled: false`) is skipped, so a disabled plugin
    /// degrades to "not applied" instead of breaking the upstream; the
    /// `upstream.auth` binding is never skipped.
    ///
    /// # Errors
    ///
    /// Returns [`OagwError::PluginNotFound`] when a binding that is not skipped
    /// (the auth binding included) references an unknown plugin, and the
    /// plugin's own error when its configuration is invalid.
    pub fn build_chain(
        &self,
        upstream: &Upstream,
        route: Option<&Route>,
        disabled_plugins: &DisabledPlugins,
    ) -> Result<PluginChain, OagwError> {
        let mut chain = PluginChain::new();
        if let Some(auth) = upstream.auth.as_ref() {
            let plugin = self.build_auth(&auth.auth_type, &auth.config)?;
            chain.push_auth(PluginTier::Upstream, 0, auth.auth_type.clone(), plugin);
        }
        for (index, binding) in upstream.plugins.items.iter().enumerate() {
            self.push_binding(
                &mut chain,
                PluginTier::Upstream,
                index,
                binding,
                disabled_plugins,
            )?;
        }
        if let Some(route) = route {
            for (index, binding) in route.plugins.items.iter().enumerate() {
                self.push_binding(
                    &mut chain,
                    PluginTier::Route,
                    index,
                    binding,
                    disabled_plugins,
                )?;
            }
        }
        Ok(chain)
    }

    fn push_binding(
        &self,
        chain: &mut PluginChain,
        tier: PluginTier,
        declaration: usize,
        binding: &PluginBinding,
        disabled_plugins: &DisabledPlugins,
    ) -> Result<(), OagwError> {
        if disabled_plugins.is_disabled(&binding.plugin_ref) {
            return Ok(());
        }
        match self.build_any(&binding.plugin_ref, &binding.config)? {
            ResolvedPlugin::Auth(plugin) => {
                chain.push_auth(tier, declaration, binding.plugin_ref.clone(), plugin);
            }
            ResolvedPlugin::Guard(plugin) => {
                chain.push_guard(tier, declaration, binding.plugin_ref.clone(), plugin);
            }
            ResolvedPlugin::Transform(plugin) => {
                chain.push_transform(tier, declaration, binding.plugin_ref.clone(), plugin);
            }
        }
        Ok(())
    }

    /// Builds — or returns the memoised — plugin instance of one binding.
    ///
    /// The memo key is the *resolved* registry key, so both spellings of a
    /// reference (the GTS id and the bare instance fragment) share one
    /// instance, which is what a stable token cache across requests needs.
    ///
    /// # Errors
    ///
    /// Returns [`OagwError::PluginNotFound`] when the id is unknown, and the
    /// plugin's own error when the configuration is invalid. A failure is never
    /// memoised: the next request constructs the plugin again.
    fn build_any(
        &self,
        plugin_ref: &str,
        config: &serde_json::Value,
    ) -> Result<ResolvedPlugin, OagwError> {
        let Some(key) = self.resolve_key(plugin_ref) else {
            return Err(plugin_not_found(plugin_ref));
        };
        let config_hash = config_hash(config);
        if let Some(cached) = self.constructed.read().get(&(key.clone(), config_hash)) {
            return Ok(cached.clone());
        }
        let built = self.construct(&key, plugin_ref, config)?;
        let mut constructed = self.constructed.write();
        if constructed.len() >= CONSTRUCTED_CAPACITY
            && !constructed.contains_key(&(key.clone(), config_hash))
        {
            constructed.clear();
        }
        constructed.insert((key, config_hash), built.clone());
        Ok(built)
    }

    /// Runs the factory of `key` once, without touching the memo table.
    fn construct(
        &self,
        key: &str,
        plugin_ref: &str,
        config: &serde_json::Value,
    ) -> Result<ResolvedPlugin, OagwError> {
        if let Some(factory) = self.auth.get(key) {
            return factory(config).map(ResolvedPlugin::Auth);
        }
        if let Some(factory) = self.guard.get(key) {
            return factory(config).map(ResolvedPlugin::Guard);
        }
        self.transform
            .get(key)
            .ok_or_else(|| plugin_not_found(plugin_ref))?(config)
        .map(ResolvedPlugin::Transform)
    }

    /// Canonical registry key of a plugin reference, trying the bare ref and
    /// then each GTS base type prefix.
    fn resolve_key(&self, plugin_ref: &str) -> Option<String> {
        if self.auth.contains_key(plugin_ref)
            || self.guard.contains_key(plugin_ref)
            || self.transform.contains_key(plugin_ref)
        {
            return Some(plugin_ref.to_owned());
        }
        for base in [
            AUTH_PLUGIN_TYPE_ID,
            GUARD_PLUGIN_TYPE_ID,
            TRANSFORM_PLUGIN_TYPE_ID,
        ] {
            let qualified = format!("{base}~{plugin_ref}");
            if self.auth.contains_key(&qualified)
                || self.guard.contains_key(&qualified)
                || self.transform.contains_key(&qualified)
            {
                return Some(qualified);
            }
        }
        None
    }
}

/// `503` problem document for an unresolvable plugin reference.
fn plugin_not_found(plugin_ref: &str) -> OagwError {
    OagwError::plugin_not_found(format!("plugin '{plugin_ref}' is not registered"))
        .with_plugin_id(plugin_ref)
}

/// Stable fingerprint of a binding configuration, the second half of the
/// memoisation key.
///
/// `serde_json::Map` is a `BTreeMap` in this workspace, so the canonical
/// serialisation is already key-sorted and two equal payloads hash equally no
/// matter the order their keys arrived in. The serialisation of a value that
/// cannot be represented (impossible for a decoded `serde_json::Value`) hashes
/// as empty, which can only ever make two such configurations share an
/// instance — never hand out a wrong one.
fn config_hash(config: &serde_json::Value) -> u64 {
    let mut hasher = DefaultHasher::new();
    serde_json::to_vec(config)
        .unwrap_or_default()
        .hash(&mut hasher);
    hasher.finish()
}

#[cfg(test)]
#[path = "plugin_tests.rs"]
mod tests;
