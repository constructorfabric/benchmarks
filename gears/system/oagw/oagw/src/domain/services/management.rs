//! Control-plane service (`FR-001`, `FR-002`).
//!
//! The service owns every cross-resource rule the pure validators cannot see:
//! plugin bindability, upstream existence, match-rule uniqueness, cascade delete
//! and the plugin-in-use guard. It is transport-free: handlers translate
//! [`DomainError`] into RFC 9457 documents at the boundary.

use std::sync::Arc;

use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::domain::model::{Plugin, PluginKind, Route, Upstream};
use crate::domain::repo::ControlPlane;
use crate::domain::type_catalog;
use crate::domain::validation::{
    ValidationContext, validate_plugin, validate_route, validate_upstream,
};

/// The tenant chain of a caller, closest tenant first.
///
/// Alias inheritance walks this chain; resource addressing never does.
pub trait TenantChain: Send + Sync {
    /// Chain for one caller.
    fn chain(&self, tenant_id: Uuid) -> Vec<Uuid>;
}

/// Single-tenant chain: the caller sees only its own resources.
#[derive(Debug, Clone, Copy, Default)]
pub struct SelfChain;

impl TenantChain for SelfChain {
    fn chain(&self, tenant_id: Uuid) -> Vec<Uuid> {
        vec![tenant_id]
    }
}

/// A tenant tree held in memory, for the deployments and tests that know their
/// hierarchy up front.
///
/// Ancestors are recorded closest first, and the chain always starts at the
/// caller itself, so a descendant never loses its own resources.
#[derive(Debug, Clone, Default)]
pub struct HierarchyChain {
    parents: std::collections::BTreeMap<Uuid, Vec<Uuid>>,
}

impl HierarchyChain {
    /// Records `ancestors` (closest first) as the chain above `tenant`.
    pub fn insert(&mut self, tenant: Uuid, ancestors: Vec<Uuid>) -> &mut Self {
        self.parents.insert(tenant, ancestors);
        self
    }
}

impl TenantChain for HierarchyChain {
    fn chain(&self, tenant_id: Uuid) -> Vec<Uuid> {
        let mut chain = vec![tenant_id];
        if let Some(ancestors) = self.parents.get(&tenant_id) {
            chain.extend(ancestors.iter().copied());
        }
        chain
    }
}

/// Pagination / ordering parameters accepted by the list endpoints.
#[derive(Debug, Clone, Default)]
pub struct ListParams {
    /// Number of entries to skip.
    pub skip: usize,
    /// Maximum number of entries to return.
    pub top: Option<usize>,
    /// Field to order by; `None` keeps insertion order.
    pub orderby: Option<String>,
}

impl ListParams {
    /// Applies `$orderby`, `$skip` and `$top` to a list.
    #[must_use]
    pub fn apply<T: Clone>(&self, mut items: Vec<T>, key: impl Fn(&T) -> (u64, u64)) -> Vec<T> {
        if let Some(spec) = &self.orderby {
            let (field, descending) = match spec.strip_suffix(" desc") {
                Some(rest) => (rest.trim(), true),
                None => (spec.trim_end_matches(" asc").trim(), false),
            };
            let index = match field {
                "updated_at" => 1_usize,
                "created_at" => 0_usize,
                _ => usize::MAX,
            };
            if index < 2 {
                items.sort_by_key(|item| {
                    let (created, updated) = key(item);
                    if index == 0 { created } else { updated }
                });
                if descending {
                    items.reverse();
                }
            }
        }
        items
            .into_iter()
            .skip(self.skip)
            .take(self.top.unwrap_or(50).min(100))
            .collect()
    }
}

/// Control-plane operations.
pub struct ManagementService {
    store: Arc<dyn ControlPlane>,
    chain: Arc<dyn TenantChain>,
    allow_http_upstream: bool,
}

impl ManagementService {
    /// Builds a service over a store.
    #[must_use]
    pub fn new(
        store: Arc<dyn ControlPlane>,
        chain: Arc<dyn TenantChain>,
        allow_http_upstream: bool,
    ) -> Self {
        Self {
            store,
            chain,
            allow_http_upstream,
        }
    }

    /// The store, for the data plane.
    #[must_use]
    pub fn store(&self) -> &Arc<dyn ControlPlane> {
        &self.store
    }

    /// The tenant chain, for callers that resolve visibility themselves.
    #[must_use]
    pub fn chain(&self) -> &Arc<dyn TenantChain> {
        &self.chain
    }

    fn context(&self, tenant_id: Uuid) -> ValidationContext {
        let mut known = type_catalog::bindable_ids();
        for plugin in self.store.plugins().list(tenant_id) {
            if let Some(id) = plugin.id {
                known.insert(id.to_string());
            }
        }
        ValidationContext {
            allow_http_upstream: self.allow_http_upstream,
            known_plugins: known,
        }
    }

    /// Creates an upstream.
    ///
    /// # Errors
    /// See [`crate::domain::validation::validate_upstream`]; also
    /// [`DomainError::AliasConflict`] on a `(tenant, alias)` collision and
    /// [`DomainError::Validation`] on an unbindable plugin reference.
    pub fn create_upstream(
        &self,
        tenant_id: Uuid,
        mut upstream: Upstream,
    ) -> Result<Upstream, DomainError> {
        upstream.tenant_id = tenant_id;
        validate_upstream(&mut upstream, &self.context(tenant_id))?;
        self.check_upstream_bindings(tenant_id, &upstream)?;
        self.store.upstreams().insert(upstream)
    }

    /// Lists the tenant's upstreams.
    #[must_use]
    pub fn list_upstreams(&self, tenant_id: Uuid, params: &ListParams) -> Vec<Upstream> {
        params.apply(self.store.upstreams().list(tenant_id), |u| {
            (u.created_at, u.updated_at)
        })
    }

    /// Reads one upstream.
    ///
    /// # Errors
    /// [`DomainError::NotFound`] when unknown or foreign.
    pub fn get_upstream(&self, tenant_id: Uuid, id: Uuid) -> Result<Upstream, DomainError> {
        self.store
            .upstreams()
            .find_by_id(tenant_id, id)
            .ok_or(DomainError::NotFound)
    }

    /// Replaces an upstream in place; the alias is immutable.
    ///
    /// # Errors
    /// [`DomainError::NotFound`], [`DomainError::Validation`].
    pub fn replace_upstream(
        &self,
        tenant_id: Uuid,
        id: Uuid,
        mut upstream: Upstream,
    ) -> Result<Upstream, DomainError> {
        let existing = self.get_upstream(tenant_id, id)?;
        upstream.tenant_id = tenant_id;
        upstream.id = Some(id);
        upstream.created_at = existing.created_at;
        validate_upstream(&mut upstream, &self.context(tenant_id))?;
        self.check_upstream_bindings(tenant_id, &upstream)?;
        self.store.upstreams().replace(tenant_id, upstream)
    }

    /// Deletes an upstream and its routes.
    ///
    /// # Errors
    /// [`DomainError::NotFound`].
    pub fn delete_upstream(
        &self,
        tenant_id: Uuid,
        id: Uuid,
    ) -> Result<(Upstream, usize), DomainError> {
        self.store.delete_upstream_cascade(tenant_id, id)
    }

    /// Creates a route.
    ///
    /// # Errors
    /// [`DomainError::NotFound`] when the upstream is unknown to the tenant,
    /// [`DomainError::Validation`] on a malformed match rule,
    /// [`DomainError::DuplicateMatchRule`] on a collision.
    pub fn create_route(&self, tenant_id: Uuid, mut route: Route) -> Result<Route, DomainError> {
        route.tenant_id = tenant_id;
        let upstream_id = route.upstream_id.ok_or(DomainError::NotFound)?;
        let upstream = self.get_upstream(tenant_id, upstream_id)?;
        validate_route(&mut route, &self.context(tenant_id))?;
        self.check_route_bindings(tenant_id, &route)?;
        validate_match_against(&route, &upstream)?;
        self.store.routes().insert(route)
    }

    /// Lists the tenant's routes.
    #[must_use]
    pub fn list_routes(&self, tenant_id: Uuid, params: &ListParams) -> Vec<Route> {
        params.apply(self.store.routes().list(tenant_id), |r| {
            (r.created_at, r.updated_at)
        })
    }

    /// Reads one route.
    ///
    /// # Errors
    /// [`DomainError::NotFound`] when unknown or foreign.
    pub fn get_route(&self, tenant_id: Uuid, id: Uuid) -> Result<Route, DomainError> {
        self.store
            .routes()
            .find_by_id(tenant_id, id)
            .ok_or(DomainError::NotFound)
    }

    /// Replaces a route; `upstream_id` is immutable.
    ///
    /// # Errors
    /// [`DomainError::NotFound`], [`DomainError::Validation`],
    /// [`DomainError::DuplicateMatchRule`].
    pub fn replace_route(
        &self,
        tenant_id: Uuid,
        id: Uuid,
        mut route: Route,
    ) -> Result<Route, DomainError> {
        let existing = self.get_route(tenant_id, id)?;
        let upstream_id = existing.upstream_id.unwrap_or_default();
        route.tenant_id = tenant_id;
        route.id = Some(id);
        route.created_at = existing.created_at;
        // `upstream_id` is absent from the update DTO; keep the stored value.
        route.upstream_id = existing.upstream_id;
        validate_route(&mut route, &self.context(tenant_id))?;
        self.check_route_bindings(tenant_id, &route)?;
        let upstream = self.get_upstream(tenant_id, upstream_id)?;
        validate_match_against(&route, &upstream)?;
        self.store.routes().replace(tenant_id, route)
    }

    /// Deletes a route; no cascade.
    ///
    /// # Errors
    /// [`DomainError::NotFound`] when unknown or foreign.
    pub fn delete_route(&self, tenant_id: Uuid, id: Uuid) -> Result<Route, DomainError> {
        self.store.routes().delete(tenant_id, id)
    }

    /// Creates a custom plugin definition.
    ///
    /// # Errors
    /// [`DomainError::Validation`] when malformed or already named.
    pub fn create_plugin(
        &self,
        tenant_id: Uuid,
        mut plugin: Plugin,
    ) -> Result<Plugin, DomainError> {
        plugin.tenant_id = tenant_id;
        plugin.id = None;
        validate_plugin(&plugin)?;
        self.store.plugins().insert(plugin)
    }

    /// Lists the tenant's custom plugins; built-ins are never listed.
    #[must_use]
    pub fn list_plugins(&self, tenant_id: Uuid, params: &ListParams) -> Vec<Plugin> {
        params.apply(self.store.plugins().list(tenant_id), |p| {
            (p.created_at, p.created_at)
        })
    }

    /// Reads one custom plugin.
    ///
    /// # Errors
    /// [`DomainError::NotFound`] when unknown, foreign, or a built-in id.
    pub fn get_plugin(&self, tenant_id: Uuid, id: Uuid) -> Result<Plugin, DomainError> {
        self.store
            .plugins()
            .find_by_id(tenant_id, id)
            .ok_or(DomainError::NotFound)
    }

    /// Deletes a custom plugin when nothing references it.
    ///
    /// # Errors
    /// [`DomainError::PluginInUse`], [`DomainError::NotFound`].
    pub fn delete_plugin(&self, tenant_id: Uuid, id: Uuid) -> Result<Plugin, DomainError> {
        if self.store.plugin_in_use(tenant_id, id) {
            return Err(DomainError::PluginInUse(id.to_string()));
        }
        self.store.plugins().delete(tenant_id, id)
    }

    fn check_upstream_bindings(
        &self,
        tenant_id: Uuid,
        upstream: &Upstream,
    ) -> Result<(), DomainError> {
        if let Some(auth) = &upstream.auth {
            self.check_auth_reference(tenant_id, &auth.kind)?;
        }
        if let Some(plugins) = &upstream.plugins {
            for item in &plugins.items {
                self.check_plugin_reference(tenant_id, item.reference())?;
            }
        }
        Ok(())
    }

    fn check_route_bindings(&self, tenant_id: Uuid, route: &Route) -> Result<(), DomainError> {
        if let Some(plugins) = &route.plugins {
            for item in &plugins.items {
                self.check_plugin_reference(tenant_id, item.reference())?;
            }
        }
        Ok(())
    }

    /// A plugin reference must be a bindable built-in or a stored custom plugin.
    fn check_plugin_reference(&self, tenant_id: Uuid, item: &str) -> Result<(), DomainError> {
        if item.starts_with("gts.") {
            if !type_catalog::is_known(item) {
                return Err(DomainError::Validation(format!(
                    "unknown_plugin: '{item}' is not in the plugin catalog"
                )));
            }
            if !type_catalog::is_bindable(item) {
                return Err(DomainError::Validation(format!(
                    "plugin_not_implemented: '{item}' has no backing implementation"
                )));
            }
            return Ok(());
        }
        match Uuid::parse_str(item) {
            Ok(id) => {
                if self.store.plugins().find_by_id(tenant_id, id).is_none() {
                    return Err(DomainError::Validation(format!(
                        "unknown_plugin: '{item}' is not a stored plugin"
                    )));
                }
                Ok(())
            }
            Err(_) => Err(DomainError::Validation(format!(
                "unknown_plugin: '{item}' is neither a built-in GTS id nor a plugin uuid"
            ))),
        }
    }

    /// An `auth.type` reference must be a bindable auth built-in or a stored
    /// auth plugin.
    fn check_auth_reference(&self, tenant_id: Uuid, kind: &str) -> Result<(), DomainError> {
        if kind.starts_with("gts.") {
            if !type_catalog::is_known(kind) {
                return Err(DomainError::Validation(format!(
                    "unknown_plugin: '{kind}' is not in the plugin catalog"
                )));
            }
            if !type_catalog::is_bindable(kind) {
                return Err(DomainError::Validation(format!(
                    "plugin_not_implemented: '{kind}' has no backing implementation"
                )));
            }
            return Ok(());
        }
        match Uuid::parse_str(kind) {
            Ok(id) => match self.store.plugins().find_by_id(tenant_id, id) {
                Some(plugin) if plugin.kind == PluginKind::Auth => Ok(()),
                Some(_) => Err(DomainError::Validation(
                    "unknown_plugin: the referenced plugin is not an auth plugin".to_owned(),
                )),
                None => Err(DomainError::Validation(format!(
                    "unknown_plugin: '{kind}' is not a stored plugin"
                ))),
            },
            Err(_) => Err(DomainError::Validation(format!(
                "unknown_plugin: '{kind}' is neither a built-in GTS id nor a plugin uuid"
            ))),
        }
    }
}

fn validate_match_against(route: &Route, upstream: &Upstream) -> Result<(), DomainError> {
    crate::domain::validation::validate_match_against_protocol(&route.match_rule, upstream.protocol)
}
