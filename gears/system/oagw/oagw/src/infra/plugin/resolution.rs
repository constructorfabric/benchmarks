//! Proxy-time plugin identifier resolution
//! (`cpt-cf-oagw-dod-plugin-system-identifier-resolution`).
//!
//! The reference a binding carries is resolved **before** execution begins, so
//! an unresolvable reference fails the request with
//! [`DomainError::PluginNotFound`] rather than being silently skipped
//! (`inst-ps-comp-8`/`-9`).
//!
//! Two scoping rules live here and must never be confused:
//!
//! * the *proxy path* walks the tenant chain, so a custom plugin record an
//!   **ancestor** bound resolves against the owning tenant's row (the DESIGN
//!   §3.2 `Proxy (data plane) — Inherited via tenant chain walk` row);
//! * the *management path* stays strictly caller-scoped and keeps returning
//!   not-found for the same ancestor-owned record.

use std::sync::Arc;

use credstore_sdk::api::CredStoreClientV1;
use uuid::Uuid;

use crate::config::TokenCacheConfig;
use crate::domain::error::DomainError;
use crate::domain::plugin::identifier::{is_catalog_only, parse_instance, PluginInstance};
use crate::domain::plugin::{AuthPlugin, GuardPlugin, PluginKind, TransformPlugin};
use crate::domain::repo::PluginRepository;
use crate::infra::plugin::registry::{AuthPluginRegistry, GuardPluginRegistry, TransformPluginRegistry};

/// The registries plus the plugin rows, resolved as one unit so a request
/// resolves its whole composed chain against one consistent view.
pub struct PluginRegistries {
    pub auth: AuthPluginRegistry,
    pub guard: GuardPluginRegistry,
    pub transform: TransformPluginRegistry,
    /// The custom-plugin rows, read through the tenant chain at proxy time.
    pub plugins: Arc<dyn PluginRepository>,
}

impl PluginRegistries {
    /// The registries over the built-in plugins and the plugin rows.
    #[must_use]
    pub fn with_builtins(
        credstore: Arc<dyn CredStoreClientV1>,
        cache_config: TokenCacheConfig,
        plugins: Arc<dyn PluginRepository>,
    ) -> Self {
        Self {
            auth: AuthPluginRegistry::with_builtins(credstore, cache_config),
            guard: GuardPluginRegistry::with_builtins(),
            transform: TransformPluginRegistry::with_builtins(),
            plugins,
        }
    }
}

/// The executable half of one resolution: the trait object the executor runs.
#[derive(Clone)]
pub enum ResolvedBinding {
    Auth(Arc<dyn AuthPlugin>),
    Guard(Arc<dyn GuardPlugin>),
    Transform(Arc<dyn TransformPlugin>),
}

/// One resolved plugin reference.
#[derive(Clone)]
pub struct ResolvedPlugin {
    /// The reference the binding carried.
    pub reference: String,
    /// The plugin type the resolution landed on.
    pub plugin_type: String,
    /// The `config_schema` a binding's instance configuration validates
    /// against.
    pub config_schema: Option<serde_json::Value>,
    /// The trait object the executor runs. `None` for a custom plugin, which
    /// is a registry reference with no backing implementation (graded
    /// deviation 6) and therefore cannot be executed.
    pub binding: Option<ResolvedBinding>,
}

impl std::fmt::Debug for ResolvedPlugin {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ResolvedPlugin")
            .field("reference", &self.reference)
            .field("plugin_type", &self.plugin_type)
            .field("executable", &self.binding.is_some())
            .finish()
    }
}

impl ResolvedPlugin {
    /// The plugin kind the resolution landed on.
    #[must_use]
    pub fn kind(&self) -> Option<PluginKind> {
        kind_of(&self.plugin_type)
    }
}

/// The plugin kind a plugin type names.
#[must_use]
pub fn kind_of(plugin_type: &str) -> Option<PluginKind> {
    match crate::domain::plugin::identifier::plugin_base_type_of(plugin_type) {
        Some(base) if base == crate::domain::gts_helpers::AUTH_PLUGIN_BASE_TYPE => {
            Some(PluginKind::Auth)
        }
        Some(base) if base == crate::domain::gts_helpers::GUARD_PLUGIN_BASE_TYPE => {
            Some(PluginKind::Guard)
        }
        Some(base) if base == crate::domain::gts_helpers::TRANSFORM_PLUGIN_BASE_TYPE => {
            Some(PluginKind::Transform)
        }
        _ => None,
    }
}

/// The tenant-chain walk a proxy-time resolution is performed over.
///
/// `tenants` is ordered base -> most specific; a UUID-backed reference bound by
/// an ancestor is looked up against each tenant in turn, which is what the
/// DESIGN §3.2 inherited-via-tenant-chain-walk row requires.
#[derive(Debug, Clone, Default)]
pub struct TenantChain {
    tenants: Vec<Uuid>,
}

impl TenantChain {
    /// A chain that walks the given tenants in order.
    #[must_use]
    pub fn new(tenants: Vec<Uuid>) -> Self {
        Self { tenants }
    }

    /// The tenants the walk consults, in order.
    #[must_use]
    pub fn tenants(&self) -> &[Uuid] {
        &self.tenants
    }
}

/// Resolve one plugin reference at proxy time
/// (`cpt-cf-oagw-algo-plugin-system-identifier-resolution`).
///
/// A catalog-only identifier never reaches a registry here: the binding-time
/// check rejects it before a binding row exists, so its presence at proxy time
/// is an invariant violation and surfaces as `PluginNotFound` too — it never
/// falls back to the core timeout, CORS, logging, or metrics behavior.
///
/// # Errors
///
/// [`DomainError::PluginNotFound`] when no registry entry and no record
/// matches the identifier.
// @cpt-begin:cpt-cf-oagw-algo-plugin-system-identifier-resolution:p1:inst-ps-res-1
// `inst-ps-res-1` .. `-11`: the instance part is parsed and classified, a UUID
// instance is resolved through the tenant-scoped repository with a matching
// plugin schema type, any other instance is resolved through the in-process
// registry of the matching type, the catalog-only set never resolves, and a
// reference nothing matches fails the request with `PluginNotFound`.
// @cpt-begin:cpt-cf-oagw-algo-plugin-system-identifier-resolution:p1:inst-ps-res-10
// @cpt-begin:cpt-cf-oagw-algo-plugin-system-identifier-resolution:p1:inst-ps-res-11
// @cpt-begin:cpt-cf-oagw-algo-plugin-system-identifier-resolution:p1:inst-ps-res-12
// @cpt-begin:cpt-cf-oagw-algo-plugin-system-identifier-resolution:p1:inst-ps-res-13
// @cpt-begin:cpt-cf-oagw-algo-plugin-system-identifier-resolution:p1:inst-ps-res-14
// @cpt-begin:cpt-cf-oagw-algo-plugin-system-identifier-resolution:p1:inst-ps-res-2
// @cpt-begin:cpt-cf-oagw-algo-plugin-system-identifier-resolution:p1:inst-ps-res-3
// @cpt-begin:cpt-cf-oagw-algo-plugin-system-identifier-resolution:p1:inst-ps-res-4
// @cpt-begin:cpt-cf-oagw-algo-plugin-system-identifier-resolution:p1:inst-ps-res-5
// @cpt-begin:cpt-cf-oagw-algo-plugin-system-identifier-resolution:p1:inst-ps-res-6
// @cpt-begin:cpt-cf-oagw-algo-plugin-system-identifier-resolution:p1:inst-ps-res-7
// @cpt-begin:cpt-cf-oagw-algo-plugin-system-identifier-resolution:p1:inst-ps-res-8
// @cpt-begin:cpt-cf-oagw-algo-plugin-system-identifier-resolution:p1:inst-ps-res-9
pub fn resolve_reference(
    registries: &PluginRegistries,
    chain: &TenantChain,
    reference: &str,
) -> Result<ResolvedPlugin, DomainError> {
    match parse_instance(reference) {
        PluginInstance::Uuid(uuid) => {
            // `inst-ps-res-13`/`-14`: a UUID-backed reference bound by an
            // ancestor resolves against the owning tenant's record through the
            // tenant-chain walk.
            for tenant in chain.tenants() {
                if let Ok(record) = registries.plugins.get(*tenant, uuid) {
                    crate::domain::plugin::identifier::validate_record_type(
                        &record.plugin_type,
                        reference,
                    )?;
                    return Ok(ResolvedPlugin {
                        reference: reference.to_owned(),
                        plugin_type: record.plugin_type.clone(),
                        config_schema: record.config_schema.clone(),
                        // A custom plugin is a registry reference with no
                        // backing implementation: it resolves, is bindable and
                        // is readable through the source endpoint, and it
                        // never executes (graded deviation 6).
                        binding: None,
                    });
                }
            }
            Err(not_found(reference))
        }
        PluginInstance::Named(name) => {
            if is_catalog_only(reference) {
                return Err(not_found(reference));
            }
            let schema = crate::infra::plugin::registry::named_config_schema(reference);
            let base = crate::domain::plugin::identifier::plugin_base_type_of(reference);
            let binding = match base {
                Some(base) if base == crate::domain::gts_helpers::AUTH_PLUGIN_BASE_TYPE => {
                    registries.auth.get(reference).ok().map(ResolvedBinding::Auth)
                }
                Some(base) if base == crate::domain::gts_helpers::GUARD_PLUGIN_BASE_TYPE => {
                    registries.guard.get(reference).ok().map(ResolvedBinding::Guard)
                }
                Some(base) if base == crate::domain::gts_helpers::TRANSFORM_PLUGIN_BASE_TYPE => {
                    registries.transform.get(reference).ok().map(ResolvedBinding::Transform)
                }
                _ => None,
            };
            let _ = &name;
            match binding {
                Some(binding) => Ok(ResolvedPlugin {
                    reference: reference.to_owned(),
                    plugin_type: reference.to_owned(),
                    config_schema: schema,
                    binding: Some(binding),
                }),
                None => Err(not_found(reference)),
            }
        }
    }
}
//
// @cpt-end:cpt-cf-oagw-algo-plugin-system-identifier-resolution:p1:inst-ps-res-9
// @cpt-end:cpt-cf-oagw-algo-plugin-system-identifier-resolution:p1:inst-ps-res-8
// @cpt-end:cpt-cf-oagw-algo-plugin-system-identifier-resolution:p1:inst-ps-res-7
// @cpt-end:cpt-cf-oagw-algo-plugin-system-identifier-resolution:p1:inst-ps-res-6
// @cpt-end:cpt-cf-oagw-algo-plugin-system-identifier-resolution:p1:inst-ps-res-5
// @cpt-end:cpt-cf-oagw-algo-plugin-system-identifier-resolution:p1:inst-ps-res-4
// @cpt-end:cpt-cf-oagw-algo-plugin-system-identifier-resolution:p1:inst-ps-res-3
// @cpt-end:cpt-cf-oagw-algo-plugin-system-identifier-resolution:p1:inst-ps-res-2
// @cpt-end:cpt-cf-oagw-algo-plugin-system-identifier-resolution:p1:inst-ps-res-14
// @cpt-end:cpt-cf-oagw-algo-plugin-system-identifier-resolution:p1:inst-ps-res-13
// @cpt-end:cpt-cf-oagw-algo-plugin-system-identifier-resolution:p1:inst-ps-res-12
// @cpt-end:cpt-cf-oagw-algo-plugin-system-identifier-resolution:p1:inst-ps-res-11
// @cpt-end:cpt-cf-oagw-algo-plugin-system-identifier-resolution:p1:inst-ps-res-10
//
// @cpt-end:cpt-cf-oagw-algo-plugin-system-identifier-resolution:p1:inst-ps-res-1

/// Resolve the auth reference the upstream `auth` block carries.
///
/// # Errors
///
/// [`DomainError::PluginNotFound`] when the block names a plugin no registry
/// resolves.
pub fn resolve_auth(
    registries: &PluginRegistries,
    chain: &TenantChain,
    auth_ref: Option<&str>,
) -> Result<Option<ResolvedPlugin>, DomainError> {
    match auth_ref {
        Some(reference) => resolve_reference(registries, chain, reference).map(Some),
        None => Ok(None),
    }
}

/// `PluginNotFound` for a reference that resolves to nothing.
fn not_found(reference: &str) -> DomainError {
    DomainError::PluginNotFound { plugin_ref: reference.to_owned() }
}

#[cfg(test)]
#[path = "identifier_resolution_tests.rs"]
mod identifier_resolution_tests;
