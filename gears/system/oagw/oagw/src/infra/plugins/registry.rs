//! In-process plugin registry, built-in catalog and custom plugin catalog.
//!
//! [`BuiltinCatalog`] is the static catalog of the PRD's plugin identifiers —
//! including the catalog-only ones (`basic`, `bearer`, `timeout`, `cors`,
//! `logging`, `metrics`) that exist for types-registry cataloging and must
//! never be bound. [`PluginRegistry`] holds the executable trait objects and
//! resolves a `plugin_ref` by full GTS identifier or short catalog name.
//! [`PluginCatalog`] is the per-tenant in-memory catalog of custom (Starlark)
//! plugin definitions the management API serves.
//!
//! [`PluginChain`] turns the `auth` block and the `plugins` chains of an
//! upstream and its route into the single execution order of
//! [ADR-0002](../../../../docs/ADR/0002-plugin-system.md): upstream plugins
//! before route plugins, one auth plugin per resource.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;

use async_trait::async_trait;
use credstore_sdk::{SecretRef, SecretValue};
use dashmap::DashMap;
use serde_json::Value;
use uuid::Uuid;

use crate::domain::error::OagwError;
use crate::domain::model::{AuthConfig, PluginBinding, PluginBindings};
use crate::infra::store::Store;

use super::auth::{
    ApiKeyAuthPlugin, BasicAuthPlugin, BearerAuthPlugin, NoopAuthPlugin, OAuth2ClientAuthMethod,
    OAuth2ClientCredAuthPlugin, TokenCacheConfig,
};
use super::guard::RequiredHeadersGuardPlugin;
use super::transform::RequestIdTransformPlugin;
use super::{
    AuthPlugin, ErrorView, GuardDecision, GuardPlugin, PluginType, RequestContext, SecretResolver,
    TransformPlugin, UpstreamResponseView,
};

// ---------------------------------------------------------------------------
// GTS identifiers of the PRD catalog
// ---------------------------------------------------------------------------

/// GTS base type every auth plugin identifier resolves under.
pub const AUTH_PLUGIN_BASE_TYPE: &str = "gts.cf.core.oagw.auth_plugin.v1~";
/// GTS base type every guard plugin identifier resolves under.
pub const GUARD_PLUGIN_BASE_TYPE: &str = "gts.cf.core.oagw.guard_plugin.v1~";
/// GTS base type every transform plugin identifier resolves under.
pub const TRANSFORM_PLUGIN_BASE_TYPE: &str = "gts.cf.core.oagw.transform_plugin.v1~";

/// Built-in guard `required_headers` — the only bindable guard
/// ([ADR-0009](../../../../docs/ADR/0009-required-headers-guard-plugin.md)).
pub const REQUIRED_HEADERS_GUARD_PLUGIN_ID: &str =
    "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1";
/// Built-in transform `request_id` — `X-Request-ID` propagation.
pub const REQUEST_ID_TRANSFORM_PLUGIN_ID: &str =
    "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.request_id.v1";

/// Catalog-only guard `timeout`: request timeouts are gear-level configuration,
/// not a [`GuardPlugin`](super::GuardPlugin).
pub const TIMEOUT_GUARD_PLUGIN_ID: &str =
    "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.timeout.v1";
/// Catalog-only guard `cors`: CORS lives in the `cors` field of an upstream or
/// route, not in a plugin chain.
pub const CORS_GUARD_PLUGIN_ID: &str = "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.cors.v1";
/// Catalog-only transform `logging`: core Data Plane instrumentation.
pub const LOGGING_TRANSFORM_PLUGIN_ID: &str =
    "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.logging.v1";
/// Catalog-only transform `metrics`: core Data Plane instrumentation.
pub const METRICS_TRANSFORM_PLUGIN_ID: &str =
    "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.metrics.v1";

// ---------------------------------------------------------------------------
// Built-in catalog
// ---------------------------------------------------------------------------

/// One entry of the PRD's built-in plugin catalog.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BuiltinDescriptor {
    /// Short catalog name (`noop`, `required_headers`, ...).
    pub name: &'static str,
    /// Full GTS identifier a binding references the plugin by.
    pub gts_id: &'static str,
    /// Kind of the plugin.
    pub plugin_type: PluginType,
    /// `true` when the plugin may appear in `auth.type` or
    /// `plugins.items[].plugin_ref`.
    pub bindable: bool,
    /// `true` when an executable implementation is registered.
    pub executable: bool,
}

/// The static catalog of the PRD's built-in plugin identifiers.
///
/// `Default` is the empty catalog: it only exists as the seed
/// [`PluginRegistry::default`] replaces in [`PluginRegistry::with_builtins`].
#[derive(Debug, Clone, Default)]
pub struct BuiltinCatalog {
    entries: Vec<BuiltinDescriptor>,
}

impl BuiltinCatalog {
    /// Builds the catalog of [`PRD.md`](../../../../docs/PRD.md)
    /// `cpt-cf-oagw-fr-builtin-plugins`, verbatim GTS identifiers included.
    #[must_use]
    pub fn with_builtins() -> Self {
        use crate::domain::model::AuthType;
        Self {
            entries: vec![
                BuiltinDescriptor {
                    name: "noop",
                    gts_id: AuthType::NOOP,
                    plugin_type: PluginType::Auth,
                    bindable: true,
                    executable: true,
                },
                BuiltinDescriptor {
                    name: "apikey",
                    gts_id: AuthType::APIKEY,
                    plugin_type: PluginType::Auth,
                    bindable: true,
                    executable: true,
                },
                BuiltinDescriptor {
                    name: "oauth2_client_cred",
                    gts_id: AuthType::OAUTH2_CLIENT_CRED,
                    plugin_type: PluginType::Auth,
                    bindable: true,
                    executable: true,
                },
                BuiltinDescriptor {
                    name: "oauth2_client_cred_basic",
                    gts_id: AuthType::OAUTH2_CLIENT_CRED_BASIC,
                    plugin_type: PluginType::Auth,
                    bindable: true,
                    executable: true,
                },
                BuiltinDescriptor {
                    name: "basic",
                    gts_id: AuthType::BASIC,
                    plugin_type: PluginType::Auth,
                    bindable: true,
                    executable: true,
                },
                BuiltinDescriptor {
                    name: "bearer",
                    gts_id: AuthType::BEARER,
                    plugin_type: PluginType::Auth,
                    bindable: true,
                    executable: true,
                },
                BuiltinDescriptor {
                    name: "required_headers",
                    gts_id: REQUIRED_HEADERS_GUARD_PLUGIN_ID,
                    plugin_type: PluginType::Guard,
                    bindable: true,
                    executable: true,
                },
                BuiltinDescriptor {
                    name: "timeout",
                    gts_id: TIMEOUT_GUARD_PLUGIN_ID,
                    plugin_type: PluginType::Guard,
                    bindable: false,
                    executable: false,
                },
                BuiltinDescriptor {
                    name: "cors",
                    gts_id: CORS_GUARD_PLUGIN_ID,
                    plugin_type: PluginType::Guard,
                    bindable: false,
                    executable: false,
                },
                BuiltinDescriptor {
                    name: "request_id",
                    gts_id: REQUEST_ID_TRANSFORM_PLUGIN_ID,
                    plugin_type: PluginType::Transform,
                    bindable: true,
                    executable: true,
                },
                BuiltinDescriptor {
                    name: "logging",
                    gts_id: LOGGING_TRANSFORM_PLUGIN_ID,
                    plugin_type: PluginType::Transform,
                    bindable: false,
                    executable: false,
                },
                BuiltinDescriptor {
                    name: "metrics",
                    gts_id: METRICS_TRANSFORM_PLUGIN_ID,
                    plugin_type: PluginType::Transform,
                    bindable: false,
                    executable: false,
                },
            ],
        }
    }

    /// Every catalog entry, in PRD order.
    #[must_use]
    pub fn entries(&self) -> &[BuiltinDescriptor] {
        &self.entries
    }

    /// The entry named by a full GTS identifier or a short catalog name.
    #[must_use]
    pub fn descriptor_for(&self, reference: &str) -> Option<&BuiltinDescriptor> {
        self.entries
            .iter()
            .find(|entry| entry.gts_id == reference || entry.name == reference)
    }

    /// The full GTS identifier `reference` names, when it is a catalog entry.
    #[must_use]
    pub fn canonical_id(&self, reference: &str) -> Option<&'static str> {
        self.descriptor_for(reference).map(|entry| entry.gts_id)
    }

    /// `true` when `name` is a built-in catalog name a custom plugin must not
    /// shadow.
    #[must_use]
    pub fn is_reserved_name(&self, name: &str) -> bool {
        self.entries.iter().any(|entry| entry.name == name)
    }

    /// Validates that `reference` names a catalog entry a resource may bind.
    ///
    /// # Errors
    /// [`OagwError::Validation`] when the reference is not in the catalog at
    /// all, or names a catalog-only identifier that is implemented as core Data
    /// Plane logic instead of as a plugin.
    pub fn require_bindable(&self, reference: &str) -> Result<&BuiltinDescriptor, OagwError> {
        let Some(descriptor) = self.descriptor_for(reference) else {
            return Err(OagwError::Validation {
                message: format!("plugin '{reference}' is not a known plugin"),
            });
        };
        if !descriptor.bindable {
            return Err(OagwError::Validation {
                message: format!(
                    "plugin '{}' is a catalog-only identifier and cannot be bound to an upstream \
                     or route",
                    descriptor.gts_id
                ),
            });
        }
        Ok(descriptor)
    }
}

// ---------------------------------------------------------------------------
// Executable registry
// ---------------------------------------------------------------------------

/// A resolved plugin, whatever its kind.
#[derive(Clone)]
pub enum PluginInstance {
    /// An executable auth plugin.
    Auth(Arc<dyn AuthPlugin>),
    /// An executable guard plugin.
    Guard(Arc<dyn GuardPlugin>),
    /// An executable transform plugin.
    Transform(Arc<dyn TransformPlugin>),
}

impl fmt::Debug for PluginInstance {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PluginInstance")
            .field("id", &self.id())
            .field("plugin_type", &self.plugin_type())
            .finish()
    }
}

impl PluginInstance {
    /// The GTS identifier the plugin is registered under.
    #[must_use]
    pub fn id(&self) -> &str {
        match self {
            Self::Auth(plugin) => plugin.id(),
            Self::Guard(plugin) => plugin.id(),
            Self::Transform(plugin) => plugin.id(),
        }
    }

    /// The kind of the plugin.
    #[must_use]
    pub const fn plugin_type(&self) -> PluginType {
        match self {
            Self::Auth(..) => PluginType::Auth,
            Self::Guard(..) => PluginType::Guard,
            Self::Transform(..) => PluginType::Transform,
        }
    }
}

/// The in-process registry of executable plugins.
///
/// Built-ins are registered by [`PluginRegistry::with_builtins`]; gears that
/// ship their own plugins add them with [`PluginRegistry::register_auth`] and
/// its siblings. Plugin state is immutable, so the registry is shared through
/// an `Arc` and every lookup is lock-free.
#[derive(Default)]
pub struct PluginRegistry {
    /// The built-in catalog this registry was seeded from.
    catalog: BuiltinCatalog,
    auth: BTreeMap<String, Arc<dyn AuthPlugin>>,
    guard: BTreeMap<String, Arc<dyn GuardPlugin>>,
    transform: BTreeMap<String, Arc<dyn TransformPlugin>>,
}

impl fmt::Debug for PluginRegistry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PluginRegistry")
            .field("catalog", &self.catalog)
            .field("auth", &self.auth.keys().collect::<Vec<_>>())
            .field("guard", &self.guard.keys().collect::<Vec<_>>())
            .field("transform", &self.transform.keys().collect::<Vec<_>>())
            .finish()
    }
}

impl PluginRegistry {
    /// Builds a registry with every built-in plugin the PRD catalogues as
    /// executable, resolving credentials through `resolver` and caching OAuth2
    /// tokens per `token_cache`.
    #[must_use]
    pub fn with_builtins(resolver: Arc<dyn SecretResolver>, token_cache: TokenCacheConfig) -> Self {
        let mut registry = Self {
            catalog: BuiltinCatalog::with_builtins(),
            ..Self::default()
        };
        registry.register_auth(Arc::new(NoopAuthPlugin));
        registry.register_auth(Arc::new(ApiKeyAuthPlugin::new(Arc::clone(&resolver))));
        registry.register_auth(Arc::new(BasicAuthPlugin::new(Arc::clone(&resolver))));
        registry.register_auth(Arc::new(BearerAuthPlugin::new(Arc::clone(&resolver))));
        registry.register_auth(Arc::new(OAuth2ClientCredAuthPlugin::new(
            Arc::clone(&resolver),
            OAuth2ClientAuthMethod::Form,
            token_cache.clone(),
        )));
        registry.register_auth(Arc::new(OAuth2ClientCredAuthPlugin::new(
            Arc::clone(&resolver),
            OAuth2ClientAuthMethod::Basic,
            token_cache,
        )));
        registry.register_guard(Arc::new(RequiredHeadersGuardPlugin));
        registry.register_transform(Arc::new(RequestIdTransformPlugin));
        registry
    }

    /// The built-in catalog this registry was seeded from.
    #[must_use]
    pub const fn catalog(&self) -> &BuiltinCatalog {
        &self.catalog
    }

    /// Registers an auth plugin under its own GTS identifier.
    pub fn register_auth(&mut self, plugin: Arc<dyn AuthPlugin>) {
        self.auth.insert(plugin.id().to_owned(), plugin);
    }

    /// Registers a guard plugin under its own GTS identifier.
    pub fn register_guard(&mut self, plugin: Arc<dyn GuardPlugin>) {
        self.guard.insert(plugin.id().to_owned(), plugin);
    }

    /// Registers a transform plugin under its own GTS identifier.
    pub fn register_transform(&mut self, plugin: Arc<dyn TransformPlugin>) {
        self.transform.insert(plugin.id().to_owned(), plugin);
    }

    /// Resolves a registered plugin by full GTS identifier or short catalog
    /// name.
    ///
    /// Built-ins are additionally known by their short catalog name; a plugin a
    /// gear registered itself is found under its own GTS identifier. Custom
    /// (UUID-backed) plugins resolve through the tenant's [`PluginCatalog`]
    /// instead: they are not registered here.
    #[must_use]
    pub fn resolve(&self, plugin_ref: &str) -> Option<PluginInstance> {
        let gts_id = self.canonical_id_of(plugin_ref);
        if let Some(plugin) = self.auth.get(gts_id.as_str()) {
            return Some(PluginInstance::Auth(Arc::clone(plugin)));
        }
        if let Some(plugin) = self.guard.get(gts_id.as_str()) {
            return Some(PluginInstance::Guard(Arc::clone(plugin)));
        }
        self.transform
            .get(gts_id.as_str())
            .map(|plugin| PluginInstance::Transform(Arc::clone(plugin)))
    }

    /// The registered auth plugin named by `plugin_ref`.
    #[must_use]
    pub fn auth_plugin(&self, plugin_ref: &str) -> Option<Arc<dyn AuthPlugin>> {
        self.auth
            .get(self.canonical_id_of(plugin_ref).as_str())
            .map(Arc::clone)
    }

    /// The GTS identifier `plugin_ref` names: the catalog entry it abbreviates,
    /// or the reference itself when it already is one.
    fn canonical_id_of(&self, plugin_ref: &str) -> String {
        self.catalog
            .canonical_id(plugin_ref)
            .map_or_else(|| plugin_ref.to_owned(), str::to_owned)
    }
}

/// Fail-closed resolver used when a gear runs without a credential store.
///
/// Any secret-dependent plugin fails with the 500
/// `cf.oagw.downstream.secret_error.v1` gateway error rather than dropping the
/// credential silently, so an operator cannot accidentally deploy an upstream
/// whose credentials are missing.
#[derive(Debug, Default)]
pub struct UnavailableSecretResolver;

#[async_trait]
impl SecretResolver for UnavailableSecretResolver {
    async fn resolve(
        &self,
        _ctx: &RequestContext,
        _secret_ref: &SecretRef,
    ) -> Result<SecretValue, OagwError> {
        Err(OagwError::SecretError {
            message: "no credential store is wired into this gear".to_owned(),
        })
    }
}

// ---------------------------------------------------------------------------
// Custom plugin catalog
// ---------------------------------------------------------------------------

/// `plugin_ref` of a custom plugin: `gts.cf.core.oagw.{type}_plugin.v1~{uuid}`
/// ([DESIGN.md](../../../../docs/DESIGN.md) "Plugin Identification Model").
#[must_use]
pub fn custom_plugin_ref(plugin_type: PluginType, id: Uuid) -> String {
    format!("{}{id}", plugin_type.base_type())
}

/// A custom plugin definition, as created through `POST /oagw/v1/plugins`.
///
/// Definitions are immutable: a change is a new plugin plus re-binding, never
/// an update.
#[derive(Debug, Clone, PartialEq)]
pub struct PluginDefinition {
    /// Server-generated id.
    pub id: Uuid,
    /// Tenant the definition belongs to.
    pub tenant_id: Uuid,
    /// Kind of the plugin.
    pub plugin_type: PluginType,
    /// Tenant-unique name.
    pub name: String,
    /// JSON Schema of the configuration the plugin accepts.
    pub config_schema: Value,
    /// Starlark source of the plugin.
    pub source_code: String,
}

impl PluginDefinition {
    /// The GTS identifier a binding references this plugin by.
    #[must_use]
    pub fn plugin_ref(&self) -> String {
        custom_plugin_ref(self.plugin_type, self.id)
    }
}

/// Validated creation input of a custom plugin.
#[derive(Debug, Clone, PartialEq)]
pub struct PluginInput {
    /// Kind of the plugin.
    pub plugin_type: PluginType,
    /// Tenant-unique name.
    pub name: String,
    /// JSON Schema of the accepted configuration.
    pub config_schema: Value,
    /// Starlark source.
    pub source_code: String,
}

/// Per-tenant in-memory catalog of custom plugin definitions, keyed per tenant
/// like the rest of the store.
#[derive(Debug, Default)]
pub struct PluginCatalog {
    plugins: DashMap<Uuid, BTreeMap<Uuid, PluginDefinition>>,
}

impl PluginCatalog {
    /// Creates an empty catalog.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Stores `input` under a server-generated id.
    pub fn create(&self, tenant_id: Uuid, input: PluginInput) -> PluginDefinition {
        let definition = PluginDefinition {
            id: Uuid::now_v7(),
            tenant_id,
            plugin_type: input.plugin_type,
            name: input.name,
            config_schema: input.config_schema,
            source_code: input.source_code,
        };
        self.plugins
            .entry(tenant_id)
            .or_default()
            .insert(definition.id, definition.clone());
        definition
    }

    /// The definition `id` of `tenant_id`, or `None` when it does not exist.
    #[must_use]
    pub fn get(&self, tenant_id: Uuid, id: Uuid) -> Option<PluginDefinition> {
        self.plugins.get(&tenant_id)?.get(&id).cloned()
    }

    /// The definition named `name` of `tenant_id`, if any.
    #[must_use]
    pub fn get_by_name(&self, tenant_id: Uuid, name: &str) -> Option<PluginDefinition> {
        self.plugins
            .get(&tenant_id)?
            .values()
            .find(|definition| definition.name == name)
            .cloned()
    }

    /// Every definition of `tenant_id`, ordered by creation.
    #[must_use]
    pub fn list(&self, tenant_id: Uuid) -> Vec<PluginDefinition> {
        self.plugins
            .get(&tenant_id)
            .map(|plugins| plugins.values().cloned().collect())
            .unwrap_or_default()
    }

    /// Deletes an unbound definition, returning whether it existed.
    pub fn delete(&self, tenant_id: Uuid, id: Uuid) -> bool {
        self.plugins
            .get_mut(&tenant_id)
            .is_some_and(|mut plugins| plugins.remove(&id).is_some())
    }

    /// Every upstream and route of `tenant_id` that still references `id`.
    ///
    /// A binding may name the bare UUID (the phase-1 [`PluginBinding`]
    /// representation) or the full GTS identifier; both count.
    #[must_use]
    pub fn usages(&self, tenant_id: Uuid, id: Uuid, store: &Store) -> Vec<PluginUsage> {
        let mut usages = Vec::new();
        for upstream in store.list_upstreams(tenant_id) {
            let Some(upstream_id) = upstream.id else {
                continue;
            };
            let auth_plugin = upstream
                .auth
                .as_ref()
                .and_then(|auth| auth.auth_type.as_ref())
                .map(crate::domain::model::AuthType::as_str);
            if auth_plugin.is_some_and(|reference| references_custom_plugin(reference, id)) {
                usages.push(PluginUsage {
                    resource: "upstream",
                    resource_id: upstream_id,
                    position: None,
                });
            }
            collect_usages(
                upstream.plugins.as_ref(),
                "upstream",
                upstream_id,
                id,
                &mut usages,
            );
        }
        for route in store.list_routes(tenant_id) {
            let Some(route_id) = route.id else {
                continue;
            };
            collect_usages(route.plugins.as_ref(), "route", route_id, id, &mut usages);
        }
        usages
    }
}

/// `true` when `binding` references the custom plugin `id`, in the bare UUID
/// form or in the full GTS form.
fn references_custom_plugin(binding: &str, id: Uuid) -> bool {
    let id = id.to_string();
    binding == id
        || binding
            .rsplit_once('~')
            .is_some_and(|(_, instance)| instance == id)
}

/// Collects the `plugins.items[]` positions that reference `id`.
fn collect_usages(
    plugins: Option<&PluginBindings>,
    resource: &'static str,
    resource_id: Uuid,
    id: Uuid,
    usages: &mut Vec<PluginUsage>,
) {
    let Some(plugins) = plugins else {
        return;
    };
    for (position, binding) in plugins.items.iter().enumerate() {
        if references_custom_plugin(binding.as_str(), id) {
            usages.push(PluginUsage {
                resource,
                resource_id,
                position: Some(position),
            });
        }
    }
}

/// One place a custom plugin is still referenced from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PluginUsage {
    /// `upstream` or `route`.
    pub resource: &'static str,
    /// Id of the referencing resource.
    pub resource_id: Uuid,
    /// Position in `plugins.items[]`; `None` when the plugin is the resource's
    /// auth plugin.
    pub position: Option<usize>,
}

impl fmt::Display for PluginUsage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.position {
            Some(position) => write!(
                f,
                "{}/{} at plugins.items[{position}]",
                self.resource, self.resource_id
            ),
            None => write!(
                f,
                "{}/{} as its auth plugin",
                self.resource, self.resource_id
            ),
        }
    }
}

// ---------------------------------------------------------------------------
// Plugin chain
// ---------------------------------------------------------------------------

/// One plugin of a chain, together with the configuration of its binding.
struct BoundPlugin<P: ?Sized> {
    plugin: Arc<P>,
    plugin_ref: String,
    config: Option<Value>,
}

impl<P: ?Sized> BoundPlugin<P> {
    /// Publishes the binding's identity and configuration on the request
    /// context, so the plugin reads its own configuration from `ctx.config`.
    fn prepare(&self, ctx: &mut RequestContext) {
        ctx.plugin_ref.clone_from(&self.plugin_ref);
        ctx.config.clone_from(&self.config);
    }
}

impl<P: ?Sized> fmt::Debug for BoundPlugin<P> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BoundPlugin")
            .field("plugin_ref", &self.plugin_ref)
            .field("config", &self.config)
            .finish()
    }
}

/// The resolved plugin chain of one request.
///
/// Order is fixed by [`PluginChain::resolve`]: the upstream's `auth` plugin
/// first, then the guards, then the transforms — upstream-bound plugins before
/// route-bound ones, and each phase running only what declared it.
#[derive(Debug, Default)]
pub struct PluginChain {
    auth: Option<BoundPlugin<dyn AuthPlugin>>,
    guards: Vec<BoundPlugin<dyn GuardPlugin>>,
    transforms: Vec<BoundPlugin<dyn TransformPlugin>>,
}

impl PluginChain {
    /// Resolves the plugin chain of one request against `registry`.
    ///
    /// `upstream_auth` carries the upstream's `auth` block, `upstream` and
    /// `route` the `plugins.items` of the two resources; the upstream's plugins
    /// are resolved first, so they run first.
    ///
    /// # Errors
    /// [`OagwError::Validation`] when a binding names an unknown plugin, a
    /// catalog-only identifier, or a second auth plugin;
    /// [`OagwError::PluginNotFound`] when a bound plugin has no in-process
    /// implementation.
    pub fn resolve(
        registry: &PluginRegistry,
        custom: Option<&PluginCatalog>,
        tenant_id: Uuid,
        upstream_auth: Option<&AuthConfig>,
        upstream: &[PluginBinding],
        route: &[PluginBinding],
    ) -> Result<Self, OagwError> {
        let mut chain = Self::default();
        if let Some(auth) = upstream_auth {
            let Some(auth_type) = auth.auth_type.as_ref() else {
                return Ok(chain);
            };
            let plugin = registry.auth_plugin(auth_type.as_str()).ok_or_else(|| {
                OagwError::PluginNotFound {
                    message: format!("auth plugin '{}' is not registered", auth_type.as_str()),
                }
            })?;
            chain.auth = Some(BoundPlugin {
                plugin,
                plugin_ref: auth_type.as_str().to_owned(),
                config: auth.config.clone(),
            });
        }
        for (scope, bindings) in [("upstream", upstream), ("route", route)] {
            for binding in bindings {
                let (instance, config) =
                    resolve_binding(registry, custom, tenant_id, binding, scope)?;
                let plugin_ref = instance.id().to_owned();
                match instance {
                    PluginInstance::Auth(..) => {
                        return Err(OagwError::Validation {
                            message: format!(
                                "plugin '{plugin_ref}' is an auth plugin: only one auth plugin \
                                 binds per resource, through 'auth.type' rather than \
                                 'plugins.items'"
                            ),
                        });
                    }
                    PluginInstance::Guard(plugin) => {
                        chain.guards.push(BoundPlugin {
                            plugin,
                            plugin_ref,
                            config,
                        });
                    }
                    PluginInstance::Transform(plugin) => {
                        chain.transforms.push(BoundPlugin {
                            plugin,
                            plugin_ref,
                            config,
                        });
                    }
                }
            }
        }
        Ok(chain)
    }

    /// `true` when no plugin is bound at all, so the chain is a no-op.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.auth.is_none() && self.guards.is_empty() && self.transforms.is_empty()
    }

    /// Runs the request side of the chain: Auth → Guards → Transform.
    ///
    /// # Errors
    /// Whatever the first failing plugin reports, which aborts the chain.
    pub async fn run_request(&self, ctx: &mut RequestContext) -> Result<GuardDecision, OagwError> {
        if let Some(bound) = &self.auth {
            bound.prepare(ctx);
            bound.plugin.authenticate(ctx).await?;
        }
        for bound in &self.guards {
            bound.prepare(ctx);
            let decision = bound.plugin.guard_request(ctx).await?;
            if !decision.is_allow() {
                return Ok(decision);
            }
        }
        for bound in &self.transforms {
            bound.prepare(ctx);
            bound.plugin.transform_request(ctx).await?;
        }
        Ok(GuardDecision::Allow)
    }

    /// Runs the response side of the chain: Transform → Guards, the reverse of
    /// the request-side onion.
    ///
    /// # Errors
    /// Whatever the first failing plugin reports.
    pub async fn run_response(
        &self,
        ctx: &mut RequestContext,
        response: &mut UpstreamResponseView,
    ) -> Result<GuardDecision, OagwError> {
        for bound in &self.transforms {
            bound.prepare(ctx);
            bound.plugin.transform_response(ctx, response).await?;
        }
        for bound in &self.guards {
            bound.prepare(ctx);
            let decision = bound.plugin.guard_response(ctx, response).await?;
            if !decision.is_allow() {
                return Ok(decision);
            }
        }
        Ok(GuardDecision::Allow)
    }

    /// Runs the error side of the chain: only transform plugins declare an
    /// error phase.
    ///
    /// # Errors
    /// Whatever the first failing plugin raises.
    pub async fn run_error(
        &self,
        ctx: &mut RequestContext,
        error: &mut ErrorView,
    ) -> Result<(), OagwError> {
        for bound in &self.transforms {
            bound.prepare(ctx);
            bound.plugin.transform_error(ctx, error).await?;
        }
        Ok(())
    }
}

/// Resolves one `plugins.items` binding of `scope` into the plugin it names and
/// that binding's configuration.
fn resolve_binding(
    registry: &PluginRegistry,
    custom: Option<&PluginCatalog>,
    tenant_id: Uuid,
    binding: &PluginBinding,
    scope: &str,
) -> Result<(PluginInstance, Option<Value>), OagwError> {
    let reference = binding.as_str();
    // Catalog-only identifiers are reserved for core Data Plane logic: they are
    // never executable and must not appear in a plugin chain.
    if let Some(descriptor) = registry.catalog().descriptor_for(reference)
        && !descriptor.bindable
    {
        return Err(OagwError::Validation {
            message: format!(
                "plugin '{reference}' is a catalog-only identifier and cannot be bound as a {scope} \
                 plugin"
            ),
        });
    }
    if let Some(instance) = registry.resolve(reference) {
        return Ok((instance, None));
    }
    if let Some(id) = binding.as_uuid() {
        return match custom.and_then(|catalog| catalog.get(tenant_id, id)) {
            Some(definition) => Err(OagwError::PluginNotFound {
                message: format!(
                    "custom plugin '{}' has no in-process implementation",
                    definition.plugin_ref()
                ),
            }),
            None => Err(OagwError::Validation {
                message: format!("plugin '{reference}' does not exist in the calling tenant"),
            }),
        };
    }
    Err(OagwError::Validation {
        message: format!("plugin '{reference}' is not a known plugin"),
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]
    use async_trait::async_trait;
    use http::StatusCode;
    use toolkit_security::SecurityContext;
    use uuid::Uuid;

    use super::{
        CORS_GUARD_PLUGIN_ID, LOGGING_TRANSFORM_PLUGIN_ID, METRICS_TRANSFORM_PLUGIN_ID,
        PluginChain, PluginRegistry, REQUEST_ID_TRANSFORM_PLUGIN_ID,
        REQUIRED_HEADERS_GUARD_PLUGIN_ID, TIMEOUT_GUARD_PLUGIN_ID, TokenCacheConfig,
        UnavailableSecretResolver,
    };
    use crate::domain::error::{OagwError, SECRET_ERROR_GTS_ID};
    use crate::domain::model::{AuthConfig, AuthType, PluginBinding, SharingMode};
    use crate::infra::plugins::{
        ErrorView, GuardDecision, GuardPlugin, PluginType, RequestContext, SecretResolver,
        TransformPlugin, UpstreamResponseView,
    };

    const CATALOG_ONLY: [&str; 4] = [
        TIMEOUT_GUARD_PLUGIN_ID,
        CORS_GUARD_PLUGIN_ID,
        LOGGING_TRANSFORM_PLUGIN_ID,
        METRICS_TRANSFORM_PLUGIN_ID,
    ];

    /// The built-ins the PRD catalogues as executable, with their short catalog
    /// name and kind.
    const EXECUTABLE: [(&str, &str, PluginType); 8] = [
        (AuthType::NOOP, "noop", PluginType::Auth),
        (AuthType::APIKEY, "apikey", PluginType::Auth),
        (
            AuthType::OAUTH2_CLIENT_CRED,
            "oauth2_client_cred",
            PluginType::Auth,
        ),
        (
            AuthType::OAUTH2_CLIENT_CRED_BASIC,
            "oauth2_client_cred_basic",
            PluginType::Auth,
        ),
        (AuthType::BASIC, "basic", PluginType::Auth),
        (AuthType::BEARER, "bearer", PluginType::Auth),
        (
            REQUIRED_HEADERS_GUARD_PLUGIN_ID,
            "required_headers",
            PluginType::Guard,
        ),
        (
            REQUEST_ID_TRANSFORM_PLUGIN_ID,
            "request_id",
            PluginType::Transform,
        ),
    ];

    /// Token cache configuration of the test registries: the tests never reach
    /// a token endpoint, so only the shape matters.
    const TOKEN_CACHE: TokenCacheConfig =
        TokenCacheConfig::new(std::time::Duration::from_secs(300), 10_000);

    // ── Test plugins that record the order they ran in ────────────────────

    /// Scratch-space key the request-side order is recorded under.
    const REQUEST_ORDER: &str = "request_order";
    /// Scratch-space key the response-side order is recorded under.
    const RESPONSE_ORDER: &str = "response_order";
    /// Scratch-space key the configuration the auth binding carried is recorded
    /// under.
    const AUTH_CONFIG: &str = "auth_config";

    const TEST_AUTH_ID: &str = "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.test_auth.v1";
    const TEST_GUARD_UPSTREAM: &str =
        "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.test_guard_upstream.v1";
    const TEST_GUARD_ROUTE: &str =
        "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.test_guard_route.v1";
    const TEST_GUARD_REJECTING: &str =
        "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.test_guard_rejecting.v1";
    const TEST_TRANSFORM_UPSTREAM: &str =
        "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.test_transform_upstream.v1";
    const TEST_TRANSFORM_ROUTE: &str =
        "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.test_transform_route.v1";

    /// Appends `label` to the `key` scratch-space entry, so a chain's order is
    /// readable as `auth,guard:upstream,...`.
    fn record(ctx: &mut RequestContext, key: &str, label: &str) {
        let next = match ctx.attribute(key) {
            Some(previous) => format!("{previous},{label}"),
            None => label.to_owned(),
        };
        ctx.set_attribute(key, next);
    }

    /// A no-op plugin of every kind, recording each phase it runs in.
    #[derive(Debug)]
    struct Recorder {
        id: &'static str,
        label: &'static str,
    }

    impl Recorder {
        fn kind(&self) -> PluginType {
            if self.id.contains("auth_plugin") {
                PluginType::Auth
            } else if self.id.contains("guard_plugin") {
                PluginType::Guard
            } else {
                PluginType::Transform
            }
        }
    }

    #[async_trait]
    impl super::AuthPlugin for Recorder {
        fn id(&self) -> &str {
            self.id
        }

        fn plugin_type(&self) -> PluginType {
            self.kind()
        }

        async fn authenticate(&self, ctx: &mut RequestContext) -> Result<(), OagwError> {
            record(ctx, REQUEST_ORDER, self.label);
            if let Some(config) = ctx.config.as_ref() {
                ctx.set_attribute(AUTH_CONFIG, config.to_string());
            }
            Ok(())
        }
    }

    #[async_trait]
    impl GuardPlugin for Recorder {
        fn id(&self) -> &str {
            self.id
        }

        fn plugin_type(&self) -> PluginType {
            self.kind()
        }

        async fn guard_request(
            &self,
            ctx: &mut RequestContext,
        ) -> Result<GuardDecision, OagwError> {
            record(ctx, REQUEST_ORDER, self.label);
            Ok(GuardDecision::Allow)
        }

        async fn guard_response(
            &self,
            ctx: &mut RequestContext,
            _response: &mut UpstreamResponseView,
        ) -> Result<GuardDecision, OagwError> {
            record(ctx, RESPONSE_ORDER, self.label);
            Ok(GuardDecision::Allow)
        }
    }

    #[async_trait]
    impl TransformPlugin for Recorder {
        fn id(&self) -> &str {
            self.id
        }

        fn plugin_type(&self) -> PluginType {
            self.kind()
        }

        async fn transform_request(&self, ctx: &mut RequestContext) -> Result<(), OagwError> {
            record(ctx, REQUEST_ORDER, self.label);
            Ok(())
        }

        async fn transform_response(
            &self,
            ctx: &mut RequestContext,
            _response: &mut UpstreamResponseView,
        ) -> Result<(), OagwError> {
            record(ctx, RESPONSE_ORDER, self.label);
            Ok(())
        }

        async fn transform_error(
            &self,
            _ctx: &mut RequestContext,
            _error: &mut ErrorView,
        ) -> Result<(), OagwError> {
            Ok(())
        }
    }

    /// A guard that rejects in the request phase: the chain must stop there.
    #[derive(Debug)]
    struct Rejecting;

    #[async_trait]
    impl GuardPlugin for Rejecting {
        fn id(&self) -> &str {
            TEST_GUARD_REJECTING
        }

        fn plugin_type(&self) -> PluginType {
            PluginType::Guard
        }

        async fn guard_request(
            &self,
            _ctx: &mut RequestContext,
        ) -> Result<GuardDecision, OagwError> {
            Ok(GuardDecision::Reject {
                status: StatusCode::BAD_REQUEST,
                error_code: "TEST_REJECTED".to_owned(),
                message: "rejected by the test guard".to_owned(),
            })
        }

        async fn guard_response(
            &self,
            _ctx: &mut RequestContext,
            _response: &mut UpstreamResponseView,
        ) -> Result<GuardDecision, OagwError> {
            Ok(GuardDecision::Allow)
        }
    }

    // ── Harness ───────────────────────────────────────────────────────────

    fn security() -> SecurityContext {
        SecurityContext::builder()
            .subject_id(Uuid::now_v7())
            .subject_tenant_id(Uuid::new_v4())
            .build()
            .expect("valid security context")
    }

    fn context() -> RequestContext {
        RequestContext::new(security(), Uuid::new_v4(), "/v1/chat")
    }

    fn empty_registry() -> PluginRegistry {
        PluginRegistry::with_builtins(std::sync::Arc::new(UnavailableSecretResolver), TOKEN_CACHE)
    }

    /// A registry with the built-ins plus one recorder of every kind, so the
    /// chain order is observable through the context attributes.
    fn recording_registry() -> PluginRegistry {
        let mut registry = empty_registry();
        registry.register_auth(std::sync::Arc::new(Recorder {
            id: TEST_AUTH_ID,
            label: "auth",
        }));
        registry.register_guard(std::sync::Arc::new(Recorder {
            id: TEST_GUARD_UPSTREAM,
            label: "guard:upstream",
        }));
        registry.register_guard(std::sync::Arc::new(Recorder {
            id: TEST_GUARD_ROUTE,
            label: "guard:route",
        }));
        registry.register_guard(std::sync::Arc::new(Rejecting));
        registry.register_transform(std::sync::Arc::new(Recorder {
            id: TEST_TRANSFORM_UPSTREAM,
            label: "transform:upstream",
        }));
        registry.register_transform(std::sync::Arc::new(Recorder {
            id: TEST_TRANSFORM_ROUTE,
            label: "transform:route",
        }));
        registry
    }

    fn auth_of(id: &str) -> Option<AuthConfig> {
        Some(AuthConfig {
            auth_type: Some(AuthType::try_new(id).expect("valid auth plugin id")),
            sharing: SharingMode::default(),
            config: None,
        })
    }

    // ── Catalog ───────────────────────────────────────────────────────────

    #[test]
    fn the_catalog_lists_every_prd_plugin_with_its_kind() {
        let catalog = PluginRegistry::with_builtins(
            std::sync::Arc::new(UnavailableSecretResolver),
            TOKEN_CACHE,
        )
        .catalog()
        .clone();

        let names: Vec<&str> = catalog.entries().iter().map(|e| e.name).collect();
        assert_eq!(
            names,
            [
                "noop",
                "apikey",
                "oauth2_client_cred",
                "oauth2_client_cred_basic",
                "basic",
                "bearer",
                "required_headers",
                "timeout",
                "cors",
                "request_id",
                "logging",
                "metrics",
            ]
        );
        assert_eq!(catalog.entries().len(), 12);
    }

    #[test]
    fn the_builtins_resolve_by_id_and_by_short_name() {
        let registry = empty_registry();
        for (id, name, expected_type) in EXECUTABLE {
            let instance = registry.resolve(id).unwrap_or_else(|| {
                panic!("built-in '{id}' is not registered");
            });
            assert_eq!(instance.id(), id);
            assert_eq!(instance.plugin_type(), expected_type, "{id}");

            let by_name = registry.resolve(name).unwrap_or_else(|| {
                panic!("built-in '{name}' is not resolvable by catalog name");
            });
            assert_eq!(by_name.id(), id, "'{name}' abbreviates {id}");
        }
    }

    #[test]
    fn catalog_only_identifiers_cannot_be_bound() {
        let catalog = empty_registry().catalog().clone();
        for id in CATALOG_ONLY {
            let error = catalog.require_bindable(id).expect_err("catalog only");
            assert!(matches!(error, OagwError::Validation { .. }), "{error}");
        }
        assert!(
            catalog
                .require_bindable("gts.cf.core.oagw.nope.v1~x.v1")
                .is_err()
        );
        for (id, ..) in EXECUTABLE {
            assert!(catalog.require_bindable(id).is_ok(), "{id}");
        }
    }

    #[test]
    fn a_custom_plugin_cannot_shadow_a_builtin_name() {
        let catalog = empty_registry().catalog().clone();
        for name in [
            "noop",
            "apikey",
            "required_headers",
            "timeout",
            "cors",
            "metrics",
        ] {
            assert!(catalog.is_reserved_name(name), "{name}");
        }
        assert!(!catalog.is_reserved_name("redact_pii"));
    }

    // ── Chain order ───────────────────────────────────────────────────────

    #[tokio::test]
    async fn the_chain_runs_auth_guards_transforms_upstream_before_route() {
        let registry = recording_registry();
        let chain = PluginChain::resolve(
            &registry,
            None,
            security().subject_tenant_id(),
            auth_of(TEST_AUTH_ID).as_ref(),
            &[
                PluginBinding::builtin(TEST_GUARD_UPSTREAM),
                PluginBinding::builtin(TEST_TRANSFORM_UPSTREAM),
            ],
            &[
                PluginBinding::builtin(TEST_GUARD_ROUTE),
                PluginBinding::builtin(TEST_TRANSFORM_ROUTE),
            ],
        )
        .expect("resolvable chain");
        assert!(!chain.is_empty());

        let mut ctx = context();
        chain.run_request(&mut ctx).await.expect("request side");

        assert_eq!(
            ctx.attribute(REQUEST_ORDER),
            Some("auth,guard:upstream,guard:route,transform:upstream,transform:route"),
            "auth, then every guard, then every transform; upstream before route within a phase"
        );
    }

    #[tokio::test]
    async fn the_response_side_runs_transforms_before_guards() {
        let registry = recording_registry();
        let chain = PluginChain::resolve(
            &registry,
            None,
            security().subject_tenant_id(),
            auth_of(TEST_AUTH_ID).as_ref(),
            &[
                PluginBinding::builtin(TEST_GUARD_UPSTREAM),
                PluginBinding::builtin(TEST_TRANSFORM_UPSTREAM),
            ],
            &[PluginBinding::builtin(TEST_TRANSFORM_ROUTE)],
        )
        .expect("resolvable chain");

        let mut ctx = context();
        let mut response = UpstreamResponseView::new(StatusCode::OK);
        chain
            .run_response(&mut ctx, &mut response)
            .await
            .expect("response side");

        assert_eq!(
            ctx.attribute(RESPONSE_ORDER),
            Some("transform:upstream,transform:route,guard:upstream"),
            "transforms run in chain order, then the guards"
        );
    }

    #[tokio::test]
    async fn an_empty_chain_is_a_noop() {
        let registry = recording_registry();
        let chain = PluginChain::resolve(
            &registry,
            None,
            security().subject_tenant_id(),
            None,
            &[],
            &[],
        )
        .expect("no plugin bound");
        assert!(chain.is_empty());

        let mut ctx = context();
        let decision = chain.run_request(&mut ctx).await.expect("request side");
        assert_eq!(decision, GuardDecision::Allow);
        assert_eq!(ctx.attribute(REQUEST_ORDER), None);
    }

    #[tokio::test]
    async fn a_guard_rejection_stops_the_chain() {
        let registry = recording_registry();
        let chain = PluginChain::resolve(
            &registry,
            None,
            security().subject_tenant_id(),
            None,
            &[PluginBinding::builtin(TEST_GUARD_REJECTING)],
            &[PluginBinding::builtin(TEST_TRANSFORM_ROUTE)],
        )
        .expect("resolvable chain");

        let mut ctx = context();
        let decision = chain.run_request(&mut ctx).await.expect("request side");
        let GuardDecision::Reject {
            status, error_code, ..
        } = decision
        else {
            panic!("expected a rejection");
        };
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(error_code, "TEST_REJECTED");
        assert_eq!(
            ctx.attribute(REQUEST_ORDER),
            None,
            "the rejecting guard runs before the route transform"
        );
    }

    #[tokio::test]
    async fn a_second_auth_plugin_in_plugins_items_is_rejected() {
        let registry = recording_registry();
        let error = PluginChain::resolve(
            &registry,
            None,
            security().subject_tenant_id(),
            None,
            &[PluginBinding::builtin(TEST_AUTH_ID)],
            &[],
        )
        .expect_err("auth plugins bind through auth.type only");
        assert!(matches!(error, OagwError::Validation { .. }), "{error}");
    }

    #[tokio::test]
    async fn the_binding_configuration_travels_on_the_request_context() {
        let registry = recording_registry();
        let config = serde_json::json!({ "secret_ref": "cred://client_secret" });
        let auth = AuthConfig {
            auth_type: Some(AuthType::try_new(TEST_AUTH_ID).expect("valid auth plugin id")),
            sharing: SharingMode::default(),
            config: Some(config),
        };
        let chain = PluginChain::resolve(
            &registry,
            None,
            security().subject_tenant_id(),
            Some(&auth),
            &[],
            &[],
        )
        .expect("resolvable chain");

        let mut ctx = context();
        chain.run_request(&mut ctx).await.expect("request side");

        assert_eq!(
            ctx.attribute(AUTH_CONFIG),
            Some("{\"secret_ref\":\"cred://client_secret\"}"),
            "the binding's config is published for the plugin currently running"
        );
    }

    // ── Binding errors ────────────────────────────────────────────────────

    #[tokio::test]
    async fn a_catalog_only_binding_is_a_400() {
        let registry = empty_registry();
        for id in CATALOG_ONLY {
            let error = PluginChain::resolve(
                &registry,
                None,
                security().subject_tenant_id(),
                None,
                &[PluginBinding::builtin(id)],
                &[],
            )
            .expect_err("catalog only");
            assert!(matches!(error, OagwError::Validation { .. }), "{error}");
        }
    }

    #[tokio::test]
    async fn an_unknown_plugin_reference_is_a_400() {
        let registry = empty_registry();
        let error = PluginChain::resolve(
            &registry,
            None,
            security().subject_tenant_id(),
            None,
            &[PluginBinding::builtin(
                "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.nope.v1",
            )],
            &[],
        )
        .expect_err("unknown");
        assert!(matches!(error, OagwError::Validation { .. }), "{error}");
    }

    #[tokio::test]
    async fn a_custom_plugin_without_an_implementation_is_a_503() {
        use crate::infra::plugins::{PluginCatalog, PluginInput};

        let registry = empty_registry();
        let catalog = PluginCatalog::new();
        let tenant = security().subject_tenant_id();
        let definition = catalog.create(
            tenant,
            PluginInput {
                plugin_type: PluginType::Transform,
                name: "redact_pii".to_owned(),
                config_schema: serde_json::json!({}),
                source_code: "def transform(ctx): pass".to_owned(),
            },
        );

        let error = PluginChain::resolve(
            &registry,
            Some(&catalog),
            tenant,
            None,
            &[PluginBinding::custom(definition.id)],
            &[],
        )
        .expect_err("no in-process implementation yet");
        assert!(matches!(error, OagwError::PluginNotFound { .. }), "{error}");

        // A UUID no definition belongs to is a 400, not a 503.
        let error = PluginChain::resolve(
            &registry,
            Some(&catalog),
            tenant,
            None,
            &[PluginBinding::custom(Uuid::new_v4())],
            &[],
        )
        .expect_err("unknown custom plugin");
        assert!(matches!(error, OagwError::Validation { .. }), "{error}");
    }

    #[test]
    fn a_custom_plugin_ref_is_the_gts_identifier_of_its_definition() {
        let id = Uuid::now_v7();
        assert_eq!(
            super::custom_plugin_ref(PluginType::Guard, id),
            format!("gts.cf.core.oagw.guard_plugin.v1~{id}")
        );
    }

    #[tokio::test]
    async fn the_fail_closed_resolver_never_yields_a_secret() {
        let resolver = UnavailableSecretResolver;
        let ctx = context();
        let reference = credstore_sdk::SecretRef::new("client_secret").expect("valid ref");
        let resolved = resolver.resolve(&ctx, &reference).await;

        let error = resolved.expect_err("the fail-closed resolver never resolves");
        assert!(matches!(error, OagwError::SecretError { .. }), "{error:?}");
        assert_eq!(error.http_status(), 500);
        assert_eq!(error.gts_id(), SECRET_ERROR_GTS_ID);
    }
}
