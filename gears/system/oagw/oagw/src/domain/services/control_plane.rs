//! The control plane: everything an operator can do to configuration.
//!
//! The service is written against the repository traits in
//! [`crate::domain::repo`] and the [`TenantHierarchy`] below, so it holds no
//! infrastructure type. Every operation is scoped by the caller's tenant
//! (`scope[0]`), a read may reach an ancestor that shares its upstreams, and a
//! write always lands on the caller's own tenant.

use std::sync::Arc;

use async_trait::async_trait;
use toolkit_security::SecurityContext;
use tracing::info;
use uuid::Uuid;

use crate::domain::alias;
use crate::domain::error::DomainError;
use crate::domain::model::{
    AuthMethod, Cors, Endpoint, HeaderTransform, LoadBalancing, PluginBinding, RateLimit, Route,
    SharingMode, Upstream,
};
use crate::domain::plugin::{PluginCatalog, PluginDescriptor};
use crate::domain::repo::{PluginRepository, RouteRepository, UpstreamRepository};

/// Resolves the tenant scope a request acts in: the caller's tenant first, then
/// its ancestors up to the root.
#[async_trait]
pub trait TenantHierarchy: Send + Sync {
    /// The scope, ordered descendant → root. Never empty for a valid subject.
    ///
    /// # Errors
    /// Returns [`ErrorKind::AuthFailed`] when the subject has no tenant.
    async fn scope(&self, ctx: &SecurityContext) -> Result<Vec<Uuid>, DomainError>;
}

/// Control plane operations over upstreams, routes and plugins.
pub struct ControlPlaneService {
    upstreams: Arc<dyn UpstreamRepository>,
    routes: Arc<dyn RouteRepository>,
    bindings: Arc<dyn PluginRepository>,
    hierarchy: Arc<dyn TenantHierarchy>,
    catalog: Arc<dyn PluginCatalog>,
    /// Catalog entries an operator has retired: no longer listed, no longer
    /// bindable, and never removed while a route still binds one.
    retired: parking_lot::RwLock<std::collections::BTreeSet<String>>,
    /// Plugins operators have filed, keyed `(tenant_id, id)`.
    custom: parking_lot::RwLock<
        std::collections::BTreeMap<(Uuid, String), crate::domain::model::CustomPlugin>,
    >,
}

impl std::fmt::Debug for ControlPlaneService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ControlPlaneService")
            .finish_non_exhaustive()
    }
}

impl ControlPlaneService {
    /// Assemble the service over the given repositories and hierarchy.
    #[must_use]
    pub fn new(
        upstreams: Arc<dyn UpstreamRepository>,
        routes: Arc<dyn RouteRepository>,
        bindings: Arc<dyn PluginRepository>,
        hierarchy: Arc<dyn TenantHierarchy>,
        catalog: Arc<dyn PluginCatalog>,
    ) -> Self {
        Self {
            upstreams,
            routes,
            bindings,
            hierarchy,
            catalog,
            retired: parking_lot::RwLock::new(std::collections::BTreeSet::new()),
            custom: parking_lot::RwLock::new(std::collections::BTreeMap::new()),
        }
    }

    /// The upstream repository, shared with the data plane.
    #[must_use]
    pub const fn upstreams(&self) -> &Arc<dyn UpstreamRepository> {
        &self.upstreams
    }

    /// The route repository, shared with the data plane for matching.
    #[must_use]
    pub const fn routes(&self) -> &Arc<dyn RouteRepository> {
        &self.routes
    }

    /// The plugin binding repository.
    #[must_use]
    pub const fn bindings(&self) -> &Arc<dyn PluginRepository> {
        &self.bindings
    }

    /// The tenant hierarchy.
    #[must_use]
    pub const fn hierarchy(&self) -> &Arc<dyn TenantHierarchy> {
        &self.hierarchy
    }

    /// The plugin catalog.
    #[must_use]
    pub const fn catalog(&self) -> &Arc<dyn PluginCatalog> {
        &self.catalog
    }

    async fn scope(&self, ctx: &SecurityContext) -> Result<Vec<Uuid>, DomainError> {
        let scope = self.hierarchy.scope(ctx).await?;
        if scope.is_empty() {
            return Err(DomainError::auth_failed("subject has no tenant"));
        }
        Ok(scope)
    }

    /// Resolve a route's `target_alias` into the upstream it names, searching
    /// the caller's scope.
    ///
    /// # Errors
    /// Returns [`ErrorKind::ResourceNotFound`] when no upstream in scope carries
    /// the alias — the route would forward to nothing.
    async fn resolve_target(&self, scope: &[Uuid], alias: &str) -> Result<Upstream, DomainError> {
        self.upstreams
            .get_by_alias(scope, alias)
            .await?
            .ok_or_else(|| {
                DomainError::not_found(format!("no upstream in scope carries the alias '{alias}'"))
            })
    }

    // -- upstreams ---------------------------------------------------------

    /// Create an upstream owned by the caller's tenant.
    ///
    /// # Errors
    /// Returns [`ErrorKind::Validation`] when the pool is empty or the alias is
    /// invalid or undervivable, and [`ErrorKind::AliasConflict`] when the alias
    /// is already taken.
    pub async fn create_upstream(
        &self,
        ctx: &SecurityContext,
        spec: UpstreamSpec,
    ) -> Result<Upstream, DomainError> {
        let scope = self.scope(ctx).await?;
        spec.validate()?;
        let alias = self.resolve_alias(&spec).await?;
        let now = crate::domain::model::now();
        let upstream = Upstream {
            id: Uuid::new_v4(),
            alias: alias.clone(),
            tenant_id: scope[0],
            created_at: now,
            updated_at: now,
            ..spec.into_upstream()
        };
        self.upstreams.insert(&upstream).await?;
        info!(tenant = %scope[0], alias = %alias, "upstream created");
        Ok(upstream)
    }

    /// Read one upstream by id within the caller's scope.
    ///
    /// # Errors
    /// Returns [`ErrorKind::ResourceNotFound`] when no such upstream exists in
    /// scope — the same answer a foreign tenant would receive.
    pub async fn get_upstream(
        &self,
        ctx: &SecurityContext,
        id: Uuid,
    ) -> Result<Upstream, DomainError> {
        let scope = self.scope(ctx).await?;
        upstream_in_scope(&self.upstreams, &scope, id).await
    }

    /// List the upstreams visible to the caller, own ones plus inherited ones.
    ///
    /// # Errors
    /// Propagates repository failures.
    pub async fn list_upstreams(
        &self,
        ctx: &SecurityContext,
    ) -> Result<Vec<Upstream>, DomainError> {
        let scope = self.scope(ctx).await?;
        self.upstreams.list_visible(&scope).await
    }

    /// Replace an upstream. The alias is immutable and `created_at` is kept.
    ///
    /// # Errors
    /// Returns [`ErrorKind::ResourceNotFound`] when the upstream is not in
    /// scope, and [`ErrorKind::Validation`] when the replacement changes the
    /// alias or is otherwise invalid.
    pub async fn replace_upstream(
        &self,
        ctx: &SecurityContext,
        id: Uuid,
        spec: UpstreamSpec,
    ) -> Result<Upstream, DomainError> {
        let scope = self.scope(ctx).await?;
        let existing = upstream_in_scope(&self.upstreams, &scope, id).await?;
        spec.validate()?;
        if let Some(supplied) = spec
            .alias
            .as_deref()
            .map(alias::normalize)
            .filter(|s| !s.is_empty())
            && supplied != existing.alias
        {
            return Err(DomainError::validation(format!(
                "alias is immutable: '{supplied}' does not match '{}'",
                existing.alias
            )));
        }
        // An ancestor disabled this pool for everyone below it; a descendant's
        // opinion does not lift that (PRD FR: enabled). A caller enabling a
        // pool that already takes traffic, or that owns the row outright, has
        // nothing to police.
        if spec.enabled && !existing.enabled && existing.tenant_id != scope[0] {
            return Err(DomainError::forbidden(format!(
                "upstream '{}' was disabled by an ancestor tenant and cannot be re-enabled here",
                existing.alias
            )));
        }
        let upstream = Upstream {
            id: existing.id,
            alias: existing.alias.clone(),
            tenant_id: existing.tenant_id,
            created_at: existing.created_at,
            updated_at: crate::domain::model::now(),
            ..spec.into_upstream()
        };
        self.upstreams.update(&upstream).await?;
        info!(tenant = %existing.tenant_id, id = %existing.id, "upstream replaced");
        Ok(upstream)
    }

    /// Delete an upstream that no route references.
    ///
    /// # Errors
    /// Returns [`ErrorKind::ResourceNotFound`] when it does not exist in scope
    /// and [`ErrorKind::ResourceInUse`] when a route still targets its alias.
    pub async fn delete_upstream(
        &self,
        ctx: &SecurityContext,
        id: Uuid,
    ) -> Result<(), DomainError> {
        let scope = self.scope(ctx).await?;
        let existing = upstream_in_scope(&self.upstreams, &scope, id).await?;
        let referencing = self
            .routes
            .routes_referencing_alias(&scope, &existing.alias)
            .await?;
        if !referencing.is_empty() {
            return Err(DomainError::resource_in_use(format!(
                "upstream '{}' is referenced by {} route(s)",
                existing.alias,
                referencing.len()
            )));
        }
        let deleted = self
            .upstreams
            .delete(existing.tenant_id, existing.id)
            .await?;
        if !deleted {
            return Err(DomainError::not_found(format!(
                "upstream {id} does not exist"
            )));
        }
        info!(tenant = %existing.tenant_id, alias = %existing.alias, "upstream deleted");
        Ok(())
    }

    /// Derive the alias from the pool, or validate the one the operator gave.
    async fn resolve_alias(&self, spec: &UpstreamSpec) -> Result<String, DomainError> {
        let supplied = spec
            .alias
            .as_deref()
            .map(alias::normalize)
            .filter(|s| !s.is_empty());
        if let Some(supplied) = supplied {
            let value = alias::from_string(&supplied)?.into_string();
            self.assert_alias_free(&value).await?;
            return Ok(value);
        }
        let derived = alias::derive(&spec.endpoints)?.ok_or_else(|| {
            DomainError::validation("no alias could be derived from the endpoint pool; supply one")
        })?;
        let value = derived.into_string();
        self.assert_alias_free(&value).await?;
        Ok(value)
    }

    /// The in-memory repository indexes every alias in the process, so a
    /// global lookup is the cheapest conflict check and never misses an alias
    /// owned by a sibling tenant.
    async fn assert_alias_free(&self, candidate: &str) -> Result<(), DomainError> {
        // A hierarchy-less implementation cannot answer a global lookup, so a
        // failed one is read as "unknown"; the repository's own insert still
        // rejects a clash.
        let known = self
            .upstreams
            .get_by_alias(&[], candidate)
            .await
            .ok()
            .flatten();
        if known.is_some() {
            return Err(DomainError::alias_conflict(format!(
                "alias '{candidate}' is already in use"
            )));
        }
        Ok(())
    }

    // -- routes ------------------------------------------------------------

    /// Create a route owned by the caller's tenant.
    ///
    /// # Errors
    /// Returns [`ErrorKind::Validation`] when the path, methods, CORS or plugin
    /// bindings are invalid, and [`ErrorKind::RouteNotFound`] when
    /// `target_alias` resolves to nothing in scope.
    pub async fn create_route(
        &self,
        ctx: &SecurityContext,
        spec: RouteSpec,
    ) -> Result<Route, DomainError> {
        let scope = self.scope(ctx).await?;
        let target = self.resolve_target(&scope, &spec.target_alias).await?;
        spec.validate()?;
        validate_cors(spec.cors.as_ref())?;
        self.validate_bindings(&scope, &spec.plugins)?;
        let now = crate::domain::model::now();
        let route = Route {
            id: Uuid::new_v4(),
            tenant_id: scope[0],
            target_alias: target.alias.clone(),
            created_at: now,
            updated_at: now,
            ..spec.into_route()
        };
        self.routes.insert(&route).await?;
        for binding in &route.plugins {
            self.bindings
                .bind(route.tenant_id, route.id, binding)
                .await?;
        }
        info!(tenant = %scope[0], path = %route.path, "route created");
        Ok(route)
    }

    /// Read one route by id within the caller's scope.
    ///
    /// # Errors
    /// Returns [`ErrorKind::ResourceNotFound`] when it does not exist.
    pub async fn get_route(&self, ctx: &SecurityContext, id: Uuid) -> Result<Route, DomainError> {
        let scope = self.scope(ctx).await?;
        route_in_scope(&self.routes, &scope, id).await
    }

    /// List the caller's own routes.
    ///
    /// # Errors
    /// Propagates repository failures.
    pub async fn list_routes(&self, ctx: &SecurityContext) -> Result<Vec<Route>, DomainError> {
        let scope = self.scope(ctx).await?;
        self.routes.list(scope[0]).await
    }

    /// Replace a route.
    ///
    /// # Errors
    /// Returns [`ErrorKind::ResourceNotFound`] when it does not exist, and
    /// [`ErrorKind::Validation`] for an invalid replacement.
    pub async fn replace_route(
        &self,
        ctx: &SecurityContext,
        id: Uuid,
        spec: RouteSpec,
    ) -> Result<Route, DomainError> {
        let scope = self.scope(ctx).await?;
        let existing = route_in_scope(&self.routes, &scope, id).await?;
        let target = self.resolve_target(&scope, &spec.target_alias).await?;
        spec.validate()?;
        validate_cors(spec.cors.as_ref())?;
        self.validate_bindings(&scope, &spec.plugins)?;
        let route = Route {
            id: existing.id,
            tenant_id: existing.tenant_id,
            target_alias: target.alias.clone(),
            created_at: existing.created_at,
            updated_at: crate::domain::model::now(),
            ..spec.into_route()
        };
        self.routes.update(&route).await?;
        for binding in &route.plugins {
            self.bindings
                .bind(route.tenant_id, route.id, binding)
                .await?;
        }
        info!(tenant = %existing.tenant_id, id = %existing.id, "route replaced");
        Ok(route)
    }

    /// Delete a route and its plugin bindings.
    ///
    /// # Errors
    /// Returns [`ErrorKind::ResourceNotFound`] when it does not exist.
    pub async fn delete_route(&self, ctx: &SecurityContext, id: Uuid) -> Result<(), DomainError> {
        let scope = self.scope(ctx).await?;
        let existing = route_in_scope(&self.routes, &scope, id).await?;
        for binding in &existing.plugins {
            self.bindings
                .unbind(existing.tenant_id, existing.id, &binding.plugin_id)
                .await?;
        }
        let deleted = self.routes.delete(existing.tenant_id, existing.id).await?;
        if !deleted {
            return Err(DomainError::not_found(format!("route {id} does not exist")));
        }
        info!(tenant = %existing.tenant_id, id = %existing.id, "route deleted");
        Ok(())
    }

    // -- plugins -----------------------------------------------------------

    /// The plugin catalog served by `GET /oagw/v1/plugins`: the shipped
    /// entries plus every custom plugin the caller's scope can see.
    pub async fn plugins(&self, ctx: &SecurityContext) -> Vec<PluginDescriptor> {
        // The read guard is dropped before the next `await`: it is not `Send`,
        // and a handler's future has to be.
        let shipped = {
            let retired = self.retired.read();
            self.catalog
                .descriptors()
                .into_iter()
                .filter(|descriptor| !retired.contains(&descriptor.id))
                .collect::<Vec<_>>()
        };
        let mut entries: Vec<PluginDescriptor> = shipped;
        for plugin in self.visible_custom(ctx).await {
            entries.push(PluginDescriptor {
                id: plugin.id,
                plugin_type: plugin.plugin_type,
                version: "1".to_owned(),
                description: plugin.description,
                built_in: false,
            });
        }
        entries
    }

    /// One plugin by identifier: shipped first, then a custom entry in scope.
    pub async fn plugin(&self, ctx: &SecurityContext, id: &str) -> Option<PluginDescriptor> {
        if self.retired.read().contains(id) {
            return None;
        }
        if let Some(descriptor) = self.catalog.descriptor(id) {
            return Some(descriptor);
        }
        self.visible_custom(ctx)
            .await
            .into_iter()
            .find(|plugin| plugin.id == id)
            .map(|plugin| PluginDescriptor {
                id: plugin.id,
                plugin_type: plugin.plugin_type,
                version: "1".to_owned(),
                description: plugin.description,
                built_in: false,
            })
    }

    /// The source of a custom plugin, for `GET /plugins/{id}/source`.
    ///
    /// # Errors
    /// Returns [`ErrorKind::ResourceNotFound`] when the identifier names no
    /// custom plugin in the caller's scope.
    pub async fn plugin_source(
        &self,
        ctx: &SecurityContext,
        id: &str,
    ) -> Result<crate::domain::model::CustomPlugin, DomainError> {
        let tenant = self.scope(ctx).await?[0];
        let found = self.custom.read().get(&(tenant, id.to_owned())).cloned();
        found.ok_or_else(|| DomainError::not_found(format!("plugin '{id}' is not in the catalog")))
    }

    /// File a custom plugin in the catalog.
    ///
    /// # Errors
    /// Returns [`ErrorKind::Validation`] when the identifier or class is
    /// unusable, and [`ErrorKind::AliasConflict`] when the identifier is taken
    /// by a shipped entry or by this tenant already.
    pub async fn create_plugin(
        &self,
        ctx: &SecurityContext,
        id: &str,
        plugin_type: crate::domain::plugin::PluginType,
        description: String,
        source: String,
    ) -> Result<crate::domain::model::CustomPlugin, DomainError> {
        let tenant = self.scope(ctx).await?[0];
        if !crate::domain::model::CustomPlugin::valid_id(id) {
            return Err(DomainError::validation(
                "a plugin id must be a non-empty identifier of at most 128 characters",
            ));
        }
        if source.trim().is_empty() {
            return Err(DomainError::validation("a plugin requires a 'source'"));
        }
        if self.catalog.descriptor(id).is_some()
            || self.custom.read().contains_key(&(tenant, id.to_owned()))
        {
            return Err(DomainError::alias_conflict(format!(
                "plugin '{id}' is already in the catalog"
            )));
        }
        let plugin = crate::domain::model::CustomPlugin {
            id: id.to_owned(),
            tenant_id: tenant,
            plugin_type,
            description,
            source,
            created_at: crate::domain::model::now(),
        };
        self.custom
            .write()
            .insert((tenant, id.to_owned()), plugin.clone());
        Ok(plugin)
    }

    /// The custom plugins visible to `ctx`: its own, plus any an ancestor
    /// shared with it through the same scope the rest of the gear reads.
    async fn visible_custom(
        &self,
        ctx: &SecurityContext,
    ) -> Vec<crate::domain::model::CustomPlugin> {
        let Ok(scope) = self.scope(ctx).await else {
            return Vec::new();
        };
        let store = self.custom.read();
        store
            .iter()
            .filter(|((tenant, _), _)| scope.contains(tenant))
            .map(|(_, plugin)| plugin.clone())
            .collect()
    }

    /// Retire a plugin: no longer listed, no longer bindable.
    ///
    /// # Errors
    /// Returns [`ErrorKind::ResourceNotFound`] when the identifier is not in
    /// the catalog, and [`ErrorKind::PluginInUse`] when a route still binds it.
    pub async fn remove_plugin(&self, ctx: &SecurityContext, id: &str) -> Result<(), DomainError> {
        let scope = self.scope(ctx).await?;
        if self.plugin(ctx, id).await.is_none() {
            return Err(DomainError::not_found(format!(
                "plugin '{id}' is not in the catalog"
            )));
        }
        let bindings = self.bindings.list_bindings(&scope, id).await?;
        if !bindings.is_empty() {
            return Err(DomainError::plugin_in_use(format!(
                "plugin '{id}' is bound to at least one route"
            )));
        }
        // A custom plugin leaves the catalog for good; a shipped one is only
        // retired, because this build still implements it.
        if let Some(custom) = {
            let tenant = scope[0];
            let mut store = self.custom.write();
            store.remove(&(tenant, id.to_owned()))
        } {
            tracing::info!(tenant = %custom.tenant_id, id = %custom.id, "custom plugin deleted");
            return Ok(());
        }
        self.retired.write().insert(id.to_owned());
        Ok(())
    }

    /// Whether `plugin_id` is referenced by any route in scope.
    ///
    /// # Errors
    /// Propagates repository failures.
    pub async fn plugin_in_use(
        &self,
        ctx: &SecurityContext,
        plugin_id: &str,
    ) -> Result<bool, DomainError> {
        let scope = self.scope(ctx).await?;
        let bindings = self.bindings.list_bindings(&scope, plugin_id).await?;
        Ok(!bindings.is_empty())
    }

    fn validate_bindings(
        &self,
        scope: &[Uuid],
        bindings: &[PluginBinding],
    ) -> Result<(), DomainError> {
        for binding in bindings {
            // A binding is only honoured for a plugin this build still ships:
            // a retired or catalogue-only identifier is rejected here, at
            // configuration time, rather than stranding every exchange.
            let custom = self
                .custom
                .read()
                .contains_key(&(scope[0], binding.plugin_id.clone()));
            // A retired entry is out of the catalog for everyone, shipped or
            // not: binding it would stand up a route the gear cannot honour.
            if self.retired.read().contains(&binding.plugin_id) {
                return Err(DomainError::plugin_not_found(format!(
                    "plugin '{}' is not in the catalog",
                    binding.plugin_id
                )));
            }
            if self.catalog.descriptor(&binding.plugin_id).is_none() && !custom {
                return Err(DomainError::plugin_not_found(format!(
                    "plugin '{}' is not in the catalog",
                    binding.plugin_id
                )));
            }
            // A custom plugin has no implementation in this build: the binding
            // stands as configuration and an exchange through it answers `503`
            // plugin-not-found, which is what the catalog's `built_in: false`
            // already told the operator.
            if !custom && !self.catalog.is_implemented(&binding.plugin_id) {
                return Err(DomainError::plugin_not_found(format!(
                    "plugin '{}' is known to the catalog but has no implementation",
                    binding.plugin_id
                )));
            }
        }
        Ok(())
    }
}

async fn upstream_in_scope(
    repo: &Arc<dyn UpstreamRepository>,
    scope: &[Uuid],
    id: Uuid,
) -> Result<Upstream, DomainError> {
    for tenant in scope {
        if let Some(upstream) = repo.get(*tenant, id).await? {
            return Ok(upstream);
        }
    }
    Err(DomainError::not_found(format!(
        "upstream {id} does not exist"
    )))
}

async fn route_in_scope(
    repo: &Arc<dyn RouteRepository>,
    scope: &[Uuid],
    id: Uuid,
) -> Result<Route, DomainError> {
    for tenant in scope {
        if let Some(route) = repo.get(*tenant, id).await? {
            return Ok(route);
        }
    }
    Err(DomainError::not_found(format!("route {id} does not exist")))
}

/// Reject a `cors` configuration that allows credentials with a wildcard origin.
///
/// # Errors
/// Returns [`ErrorKind::Validation`] for the forbidden combination.
pub fn validate_cors(cors: Option<&Cors>) -> Result<(), DomainError> {
    let Some(cors) = cors else {
        return Ok(());
    };
    if cors.allow_credentials && cors.allow_origins.iter().any(|origin| origin == "*") {
        return Err(DomainError::validation(
            "cors.allow_credentials cannot be combined with a wildcard origin",
        ));
    }
    Ok(())
}

/// An upstream as supplied by an operator.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UpstreamSpec {
    /// Alias, when the operator supplies one instead of deriving it.
    pub alias: Option<String>,
    /// Human label.
    pub name: String,
    /// Free-text description.
    pub description: String,
    /// Endpoint pool.
    pub endpoints: Vec<Endpoint>,
    /// Load-balancing strategy across the pool.
    pub load_balancing: LoadBalancing,
    /// Credential injection methods.
    pub auth_methods: Vec<AuthMethod>,
    /// Add-only labels.
    pub tags: Vec<String>,
    /// Visibility to descendants.
    pub sharing: SharingMode,
    /// Rate limit for the pool.
    pub rate_limit: Option<RateLimit>,
    /// Whether the pool accepts traffic at all.
    pub enabled: bool,
}

impl UpstreamSpec {
    /// Turn the spec into an upstream; the caller supplies identity.
    #[must_use]
    pub fn into_upstream(self) -> Upstream {
        let now = time::OffsetDateTime::now_utc();
        Upstream {
            id: Uuid::new_v4(),
            alias: String::new(),
            name: self.name,
            description: self.description,
            tenant_id: Uuid::nil(),
            endpoints: self.endpoints,
            load_balancing: self.load_balancing,
            auth_methods: self.auth_methods,
            tags: self.tags,
            sharing: self.sharing,
            rate_limit: self.rate_limit,
            enabled: self.enabled,
            created_at: now,
            updated_at: now,
        }
    }

    /// Validate the spec before it becomes an upstream.
    ///
    /// # Errors
    /// Returns [`ErrorKind::Validation`] when the pool is empty, an endpoint
    /// has no host, or an auth method names no credential.
    pub fn validate(&self) -> Result<(), DomainError> {
        if self.endpoints.is_empty() {
            return Err(DomainError::validation("at least one endpoint is required"));
        }
        for endpoint in &self.endpoints {
            if endpoint.host.trim().is_empty() {
                return Err(DomainError::validation("every endpoint requires a host"));
            }
        }
        validate_pool_agreement(&self.endpoints)?;
        validate_auth_methods(&self.auth_methods)
    }
}

/// Check that one upstream's endpoints agree with each other.
///
/// The pool is load-balanced as one destination, so its members must be
/// interchangeable: one scheme and one explicit port. A pool that mixes them
/// would send the same call to two different services depending on the draw.
///
/// # Errors
/// Returns [`ErrorKind::Validation`] naming the disagreement.
pub fn validate_pool_agreement(
    endpoints: &[crate::domain::model::Endpoint],
) -> Result<(), DomainError> {
    let Some(first) = endpoints.first() else {
        return Ok(());
    };
    for endpoint in endpoints {
        if endpoint.scheme != first.scheme {
            return Err(DomainError::validation(format!(
                "every endpoint in a pool must agree on the scheme: '{}://{host}' and '{}://{first}' do not",
                endpoint.scheme.as_str(),
                first.scheme.as_str(),
                host = endpoint.host,
                first = first.host
            )));
        }
        match (endpoint.port, first.port) {
            (Some(mine), Some(theirs)) if mine != theirs => {
                return Err(DomainError::validation(format!(
                    "every endpoint in a pool must agree on an explicitly configured port: '{host}' uses {mine} and '{first}' uses {theirs}",
                    host = endpoint.host,
                    first = first.host
                )));
            }
            _ => {}
        }
    }
    Ok(())
}

/// Check that every auth method declares the credential it needs.
///
/// # Errors
/// Returns [`ErrorKind::Validation`] naming the missing field, so an operator
/// can complete the configuration.
pub fn validate_auth_methods(
    methods: &[crate::domain::model::AuthMethod],
) -> Result<(), DomainError> {
    use crate::domain::model::AuthMethod;
    for method in methods {
        match method {
            AuthMethod::ApiKey { secret_ref, .. } if secret_ref.trim().is_empty() => {
                return Err(DomainError::validation(
                    "an api_key auth method requires a 'secret_ref'",
                ));
            }
            AuthMethod::OAuth2ClientCredentials {
                token_url,
                client_id_ref,
                client_secret_ref,
                ..
            } => {
                if token_url.trim().is_empty() {
                    return Err(DomainError::validation(
                        "an oauth2_client_credentials auth method requires a 'token_url'",
                    ));
                }
                if client_id_ref.trim().is_empty() || client_secret_ref.trim().is_empty() {
                    return Err(DomainError::validation(
                        "an oauth2_client_credentials auth method requires a 'client_id_ref' and \
                         a 'client_secret_ref'",
                    ));
                }
            }
            AuthMethod::ApiKey { .. } => {}
        }
    }
    Ok(())
}

/// A route as supplied by an operator.
///
/// The three switches are independent on the wire, exactly as the API contract
/// spells them, so they stay three fields rather than a fold.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools)]
pub struct RouteSpec {
    /// Path pattern.
    pub path: String,
    /// Allowed methods.
    pub methods: Vec<String>,
    /// Upstream alias to forward to.
    pub target_alias: String,
    /// Prefix prepended to the forwarded path.
    pub target_path_prefix: String,
    /// Whether the matched prefix is stripped from the forwarded path.
    pub strip_prefix: bool,
    /// Whether the client's `Host` is forwarded.
    pub preserve_host: bool,
    /// Request header transformations.
    pub request_headers: Vec<HeaderTransform>,
    /// Response header transformations.
    pub response_headers: Vec<HeaderTransform>,
    /// Per-route timeout override in seconds.
    pub timeout_secs: Option<u64>,
    /// Per-route rate limit.
    pub rate_limit: Option<RateLimit>,
    /// Plugins bound to the route.
    pub plugins: Vec<PluginBinding>,
    /// CORS configuration.
    pub cors: Option<Cors>,
    /// Ordering among equally specific matches.
    pub priority: u32,
    /// Whether the route participates in matching.
    pub enabled: bool,
    /// What happens to the path beyond the route's own.
    pub path_suffix_mode: crate::domain::model::PathSuffixMode,
    /// Which of the caller's own headers the upstream may see.
    pub passthrough: crate::domain::model::Passthrough,
    /// Headers forwarded when the mode is `allowlist`.
    pub passthrough_allowlist: Vec<String>,
    /// Add-only labels.
    pub tags: Vec<String>,
}

impl RouteSpec {
    /// Turn the spec into a route; the caller supplies identity and target.
    #[must_use]
    pub fn into_route(self) -> Route {
        let now = time::OffsetDateTime::now_utc();
        Route {
            id: Uuid::new_v4(),
            tenant_id: Uuid::nil(),
            path: self.path,
            methods: self.methods,
            target_alias: self.target_alias,
            target_path_prefix: self.target_path_prefix,
            strip_prefix: self.strip_prefix,
            preserve_host: self.preserve_host,
            request_headers: self.request_headers,
            response_headers: self.response_headers,
            timeout_secs: self.timeout_secs,
            rate_limit: self.rate_limit,
            plugins: self.plugins,
            cors: self.cors,
            priority: self.priority,
            enabled: self.enabled,
            path_suffix_mode: self.path_suffix_mode,
            passthrough: self.passthrough,
            passthrough_allowlist: self.passthrough_allowlist,
            tags: self.tags,
            created_at: now,
            updated_at: now,
        }
    }

    /// Validate the spec before it is persisted.
    ///
    /// # Errors
    /// Returns [`ErrorKind::Validation`] when the path does not start with `/`
    /// or a method token is not an HTTP method.
    pub fn validate(&self) -> Result<(), DomainError> {
        if !self.path.starts_with('/') {
            return Err(DomainError::validation("route path must start with '/'"));
        }
        if self.methods.is_empty() {
            return Err(DomainError::validation(
                "route must allow at least one method",
            ));
        }
        for method in &self.methods {
            if method != "*" && http::Method::from_bytes(method.as_bytes()).is_err() {
                return Err(DomainError::validation(format!(
                    "'{method}' is not an HTTP method token"
                )));
            }
        }
        validate_transforms("request_headers", &self.request_headers)?;
        validate_transforms("response_headers", &self.response_headers)
    }
}

/// Check that a route's header transformations name real headers and name a
/// value to apply.
///
/// # Errors
/// Returns [`ErrorKind::Validation`] naming the offending header, so a
/// configuration that would fail on the first request fails before it is kept.
pub fn validate_transforms(
    field: &str,
    transforms: &[crate::domain::model::HeaderTransform],
) -> Result<(), DomainError> {
    use crate::domain::model::HeaderAction;
    for transform in transforms {
        if http::HeaderName::from_bytes(transform.name.as_bytes()).is_err() {
            return Err(DomainError::validation(format!(
                "{field} carries '{name}', which is not a valid header name",
                name = transform.name
            )));
        }
        if transform.action == HeaderAction::Remove {
            continue;
        }
        if transform.value.trim().is_empty() && transform.value_ref.trim().is_empty() {
            return Err(DomainError::validation(format!(
                "{field} needs a 'value' or a 'value_ref' on '{}'",
                transform.name
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod upstream_spec_tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use crate::domain::error::ErrorKind;
    use crate::domain::model::{Endpoint, Scheme};

    /// An endpoint on `host`, with everything else defaulted.
    fn endpoint(scheme: Scheme, host: &str, port: Option<u16>) -> Endpoint {
        Endpoint {
            scheme,
            host: host.to_owned(),
            port,
            ..Endpoint::default()
        }
    }

    #[test]
    fn a_mixed_scheme_pool_is_refused() {
        let spec = UpstreamSpec {
            name: "mixed".to_owned(),
            endpoints: vec![
                endpoint(Scheme::Https, "api.partner.com", None),
                endpoint(Scheme::Http, "api.partner.com", None),
            ],
            ..UpstreamSpec::default()
        };
        let err = spec.validate().unwrap_err();
        assert_eq!(err.kind(), ErrorKind::Validation);
        assert!(
            err.detail().contains("scheme"),
            "the refusal names the disagreement: {err}"
        );
    }

    #[test]
    fn a_pool_that_disagrees_on_an_explicit_port_is_refused() {
        let spec = UpstreamSpec {
            name: "ports".to_owned(),
            endpoints: vec![
                endpoint(Scheme::Https, "api.partner.com", Some(8443)),
                endpoint(Scheme::Https, "api.partner.com", Some(9443)),
            ],
            ..UpstreamSpec::default()
        };
        let err = spec.validate().unwrap_err();
        assert_eq!(err.kind(), ErrorKind::Validation);
        assert!(
            err.detail().contains("port"),
            "the refusal names the disagreement: {err}"
        );
    }

    #[test]
    fn a_uniform_pool_is_accepted() {
        let spec = UpstreamSpec {
            name: "uniform".to_owned(),
            endpoints: vec![
                endpoint(Scheme::Https, "a.partner.com", None),
                endpoint(Scheme::Https, "b.partner.com", Some(8443)),
                endpoint(Scheme::Https, "c.partner.com", Some(8443)),
            ],
            ..UpstreamSpec::default()
        };
        assert!(spec.validate().is_ok(), "same scheme, same explicit port");
    }

    /// `http` is a legal endpoint scheme; a uniform plaintext pool is accepted
    /// by the model and only gated by the deployment's transport posture.
    #[test]
    fn a_uniform_http_pool_is_accepted() {
        let spec = UpstreamSpec {
            name: "plaintext".to_owned(),
            endpoints: vec![
                endpoint(Scheme::Http, "a.internal", None),
                endpoint(Scheme::Http, "b.internal", Some(8080)),
                endpoint(Scheme::Http, "c.internal", Some(8080)),
            ],
            ..UpstreamSpec::default()
        };
        assert!(spec.validate().is_ok());
    }

    /// A port nobody set is not a disagreement: two endpoints that both fall
    /// back to the scheme default agree without saying so.
    #[test]
    fn two_defaulted_ports_agree() {
        let spec = UpstreamSpec {
            name: "defaults".to_owned(),
            endpoints: vec![
                endpoint(Scheme::Https, "a.partner.com", None),
                endpoint(Scheme::Https, "b.partner.com", None),
            ],
            ..UpstreamSpec::default()
        };
        assert!(spec.validate().is_ok());
    }
}
