// Created: 2026-09-04 by Constructor Tech
//! In-memory, tenant-scoped store of the OAGW control plane
//! (`docs/ADR/0006-state-management.md` §"CP State").
//!
//! The crate has no `toolkit-db` dependency, so this store *is* the source of
//! truth: one short-lived [`parking_lot::RwLock`] guards the whole state, which
//! keeps every invariant (alias uniqueness, referential integrity, plugin
//! use) atomic without ever holding a lock across an `await` — the control
//! plane service is entirely synchronous.

use parking_lot::RwLock;
use uuid::Uuid;

use crate::domain::{Alias, Plugin, Route, Upstream};

/// Everything the control plane owns, in registration order.
///
/// Registration order is part of the observable contract: list endpoints
/// return resources in the order they were created and `docs/PRD.md` §5.5
/// breaks route-match ties by "first registered route wins".
#[derive(Debug, Default, Clone)]
pub struct StoreState {
    /// Upstreams registered per tenant.
    pub upstreams: Vec<Upstream>,
    /// Routes registered per tenant.
    pub routes: Vec<Route>,
    /// Custom (Starlark) plugins registered per tenant.
    pub plugins: Vec<Plugin>,
}

impl StoreState {
    /// The tenant's upstream with `id`, if it exists.
    ///
    /// Ancestor resources are invisible to this lookup
    /// (`docs/DESIGN.md` §3.3 "Tenant Scoping"): the tenant must match.
    #[must_use]
    pub fn upstream(&self, tenant_id: Uuid, id: Uuid) -> Option<&Upstream> {
        self.upstreams
            .iter()
            .find(|upstream| upstream.id == id && upstream.tenant_id == tenant_id)
    }

    /// The tenant's route with `id`.
    #[must_use]
    pub fn route(&self, tenant_id: Uuid, id: Uuid) -> Option<&Route> {
        self.routes
            .iter()
            .find(|route| route.id == id && route.tenant_id == tenant_id)
    }

    /// The tenant's custom plugin with `id`.
    #[must_use]
    pub fn plugin(&self, tenant_id: Uuid, id: Uuid) -> Option<&Plugin> {
        self.plugins
            .iter()
            .find(|plugin| plugin.id == id && plugin.tenant_id == tenant_id)
    }

    /// Upstream of the tenant that already holds `alias`
    /// (`docs/PRD.md` §5.5: aliases are unique per `(tenant_id, alias)`).
    #[must_use]
    pub fn alias_holder(&self, tenant_id: Uuid, alias: &Alias) -> Option<Uuid> {
        self.upstreams
            .iter()
            .find(|upstream| upstream.tenant_id == tenant_id && &upstream.alias == alias)
            .map(|upstream| upstream.id)
    }

    /// Same as [`StoreState::alias_holder`], ignoring `excluded_id` (the
    /// upstream being replaced).
    #[must_use]
    pub fn alias_holder_excluding(
        &self,
        tenant_id: Uuid,
        alias: &Alias,
        excluded_id: Uuid,
    ) -> Option<Uuid> {
        self.upstreams
            .iter()
            .find(|upstream| {
                upstream.tenant_id == tenant_id
                    && upstream.id != excluded_id
                    && &upstream.alias == alias
            })
            .map(|upstream| upstream.id)
    }

    /// Routes of the tenant whose match keys are identical to `key` on
    /// `upstream_id` (`docs/DESIGN.md` §3.3 "POST (Create)": match rule
    /// uniqueness within the upstream).
    #[must_use]
    pub fn route_with_match_key(
        &self,
        tenant_id: Uuid,
        upstream_id: Uuid,
        match_key: &str,
    ) -> Option<Uuid> {
        self.routes
            .iter()
            .find(|route| {
                route.tenant_id == tenant_id
                    && route.upstream_id == upstream_id
                    && match_key_of(route) == match_key
            })
            .map(|route| route.id)
    }

    /// Custom plugin of the tenant already registered under `name`.
    #[must_use]
    pub fn plugin_by_name(&self, tenant_id: Uuid, name: &str) -> Option<&Plugin> {
        self.plugins
            .iter()
            .find(|plugin| plugin.tenant_id == tenant_id && plugin.name == name)
    }

    /// Human-readable description of the first resource that still references
    /// `plugin_id`, or `None` when the plugin is unused.
    #[must_use]
    pub fn plugin_used_by(&self, tenant_id: Uuid, plugin_id: Uuid) -> Option<String> {
        // A custom plugin may be referenced either by its bare UUID or by the
        // full GTS instance id of `docs/DESIGN.md` §3.1, so both spellings must
        // count as a use.
        let gts_instance = self
            .plugin(tenant_id, plugin_id)
            .map(|plugin| format!("{}{}", plugin.gts_type(), plugin.id.as_simple()))?;
        let references = |reference: &crate::domain::PluginRef| {
            reference.uuid_matches(plugin_id) || reference.as_ref_str().as_ref() == gts_instance
        };
        let referenced = |chain: Option<&crate::domain::PluginChain>| {
            chain.is_some_and(|chain| chain.items.iter().any(references))
        };
        self.upstreams
            .iter()
            .filter(|upstream| upstream.tenant_id == tenant_id)
            .find(|upstream| {
                referenced(upstream.plugins.as_ref())
                    || upstream
                        .auth
                        .as_ref()
                        .and_then(|auth| auth.plugin.as_ref())
                        .is_some_and(references)
            })
            .map(|upstream| format!("upstream {}", upstream.id))
            .or_else(|| {
                self.routes
                    .iter()
                    .filter(|route| route.tenant_id == tenant_id)
                    .find(|route| referenced(route.plugins.as_ref()))
                    .map(|route| format!("route {}", route.id))
            })
    }

    /// Human-readable description of the first route that still points at
    /// `upstream_id`, or `None` when the upstream is unreferenced.
    #[must_use]
    pub fn upstream_used_by(&self, tenant_id: Uuid, upstream_id: Uuid) -> Option<String> {
        self.routes
            .iter()
            .filter(|route| route.tenant_id == tenant_id)
            .find(|route| route.upstream_id == upstream_id)
            .map(|route| format!("route {}", route.id))
    }
}

/// Canonical identity of a route's match keys: the unit of match-rule
/// uniqueness within an upstream (`docs/DESIGN.md` §3.3). HTTP routes are
/// keyed by their path prefix plus their method set, gRPC routes by
/// `(service, method)`.
#[must_use]
pub fn match_key_of(route: &Route) -> String {
    match &route.r#match {
        crate::domain::RouteMatch::Http(http) => {
            let mut methods: Vec<String> = http
                .methods
                .iter()
                .map(|method| method.as_str().to_owned())
                .collect();
            methods.sort_unstable();
            format!("http {} {}", http.path, methods.join(","))
        }
        crate::domain::RouteMatch::Grpc(grpc) => {
            format!("grpc {} {}", grpc.service, grpc.method)
        }
    }
}

/// In-memory store of the control plane: the authority the data plane
/// resolves configurations from (`docs/ADR/0006-state-management.md`).
#[derive(Debug, Default)]
pub struct ControlPlaneStore {
    state: RwLock<StoreState>,
}

impl ControlPlaneStore {
    /// Creates an empty store.
    #[must_use]
    pub fn new() -> Self {
        Self {
            state: RwLock::new(StoreState::default()),
        }
    }

    /// Runs `read` against a consistent snapshot of the state.
    ///
    /// The lock is taken for the duration of one synchronous call only, so it
    /// is never held across an `await`.
    #[must_use]
    pub fn read<R>(&self, read: impl FnOnce(&StoreState) -> R) -> R {
        let guard = self.state.read();
        read(&guard)
    }

    /// Runs `write` against the mutable state, so a read-modify-write
    /// invariant (alias uniqueness, referential integrity) is atomic.
    pub fn write<R>(&self, write: impl FnOnce(&mut StoreState) -> R) -> R {
        let mut guard = self.state.write();
        write(&mut guard)
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "store_tests.rs"]
mod store_tests;
