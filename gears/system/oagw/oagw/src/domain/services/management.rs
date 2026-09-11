// Updated: 2026-09-01 by Constructor Tech
//! Control Plane service: upstreams, routes and plugins.
//!
//! Every method takes the caller's [`SecurityContext`], resolves the caller's
//! tenant, and enforces the authorization scope before touching the store.
//! Resources belonging to another tenant are reported as *not found* rather
//! than forbidden, so the API does not disclose existence across tenants.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::SystemTime;

use authz_resolver_sdk::pep::{PolicyEnforcer, ResourceType};
use tenant_resolver_sdk::TenantResolverClient;
use tenant_resolver_sdk::models::BarrierMode;
use toolkit_security::SecurityContext;
use toolkit_security::pep_properties;
use uuid::Uuid;

use crate::config::OagwConfig;
use crate::domain::dto::{self, Plugin, PluginKind, Route, Upstream};
use crate::domain::error::DomainError;
use crate::domain::repo::{PluginRecord, RouteRecord, UpstreamRecord};
use crate::infra::plugin::registry::PluginRegistry;
use crate::infra::proxy::alias;
use crate::infra::storage::memory::Repos;

// ── Authorization descriptors ───────────────────────────────────────────────

/// Actions used with the `PolicyEnforcer`. DESIGN's permission table names
/// `create;override;read;delete` for upstreams and routes and
/// `create;read;delete` for plugins.
pub mod action {
    pub const CREATE: &str = "create";
    pub const READ: &str = "read";
    pub const OVERRIDE: &str = "override";
    pub const DELETE: &str = "delete";
    /// The proxy API's single permission.
    pub const INVOKE: &str = "invoke";
}

// These are `static` rather than `const` so `plugin_resource` can hand back a
// reference with a `'static` lifetime: a `const` is inlined at each use site
// and the reference would point at a temporary.
static UPSTREAM_RESOURCE: ResourceType = ResourceType::from_static(
    crate::gts::UPSTREAM_TYPE,
    &[pep_properties::OWNER_TENANT_ID],
);
static ROUTE_RESOURCE: ResourceType =
    ResourceType::from_static(crate::gts::ROUTE_TYPE, &[pep_properties::OWNER_TENANT_ID]);
static AUTH_PLUGIN_RESOURCE: ResourceType = ResourceType::from_static(
    crate::gts::AUTH_PLUGIN_TYPE,
    &[pep_properties::OWNER_TENANT_ID],
);
static GUARD_PLUGIN_RESOURCE: ResourceType = ResourceType::from_static(
    crate::gts::GUARD_PLUGIN_TYPE,
    &[pep_properties::OWNER_TENANT_ID],
);
static TRANSFORM_PLUGIN_RESOURCE: ResourceType = ResourceType::from_static(
    crate::gts::TRANSFORM_PLUGIN_TYPE,
    &[pep_properties::OWNER_TENANT_ID],
);
static PROXY_RESOURCE: ResourceType =
    ResourceType::from_static(crate::gts::PROXY_TYPE, &[pep_properties::OWNER_TENANT_ID]);

fn plugin_resource(kind: PluginKind) -> &'static ResourceType {
    match kind {
        PluginKind::Auth => &AUTH_PLUGIN_RESOURCE,
        PluginKind::Guard => &GUARD_PLUGIN_RESOURCE,
        PluginKind::Transform => &TRANSFORM_PLUGIN_RESOURCE,
    }
}

// ── Service ─────────────────────────────────────────────────────────────────

/// Control Plane service.
pub struct ManagementService {
    repos: Repos,
    tenants: Option<Arc<dyn TenantResolverClient>>,
    enforcer: Option<Arc<PolicyEnforcer>>,
    registry: Arc<PluginRegistry>,
    config: OagwConfig,
}

impl std::fmt::Debug for ManagementService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ManagementService").finish_non_exhaustive()
    }
}

impl ManagementService {
    #[must_use]
    pub fn new(
        repos: Repos,
        tenants: Option<Arc<dyn TenantResolverClient>>,
        enforcer: Option<Arc<PolicyEnforcer>>,
        registry: Arc<PluginRegistry>,
        config: OagwConfig,
    ) -> Self {
        Self {
            repos,
            tenants,
            enforcer,
            registry,
            config,
        }
    }

    #[must_use]
    pub fn registry(&self) -> &Arc<PluginRegistry> {
        &self.registry
    }

    #[must_use]
    pub fn config(&self) -> &OagwConfig {
        &self.config
    }

    // ── Authorization ───────────────────────────────────────────────────

    /// Tenants the caller may see, or `None` for "every tenant".
    ///
    /// An empty set means the PDP denied outright.
    async fn visible_tenants(
        &self,
        ctx: &SecurityContext,
        resource: &ResourceType,
        act: &str,
        id: Option<Uuid>,
    ) -> Result<Option<HashSet<Uuid>>, DomainError> {
        let Some(enforcer) = &self.enforcer else {
            return Ok(None);
        };
        let scope = enforcer
            .access_scope(ctx, resource, act, id)
            .await
            .map_err(|e| match e {
                authz_resolver_sdk::pep::EnforcerError::Denied { .. } => DomainError::Forbidden,
                other => DomainError::Internal(other.to_string()),
            })?;
        if scope.is_deny_all() {
            return Ok(Some(HashSet::new()));
        }
        if scope.is_unconstrained() {
            return Ok(None);
        }
        let values = scope.all_uuid_values_for(pep_properties::OWNER_TENANT_ID);
        if values.is_empty() {
            return Ok(None);
        }
        Ok(Some(values.into_iter().collect()))
    }

    /// The tenant a write is attributed to: always the caller's own tenant.
    ///
    /// Strictly own-tenant: an ancestor's resources are reachable for reading
    /// and proxying, but never writable by a descendant.
    async fn own_tenant(&self, ctx: &SecurityContext) -> Result<Uuid, DomainError> {
        let tenant = ctx.subject_tenant_id();
        if tenant.is_nil() {
            return Err(DomainError::NoTenant);
        }
        Ok(tenant)
    }

    async fn check_write(
        &self,
        ctx: &SecurityContext,
        resource: &ResourceType,
        act: &str,
        id: Option<Uuid>,
    ) -> Result<Uuid, DomainError> {
        let tenant = self.own_tenant(ctx).await?;
        if let Some(allowed) = self.visible_tenants(ctx, resource, act, id).await?
            && !allowed.contains(&tenant)
        {
            return Err(DomainError::Forbidden);
        }
        Ok(tenant)
    }

    async fn check_read(
        &self,
        ctx: &SecurityContext,
        resource: &ResourceType,
        id: Option<Uuid>,
    ) -> Result<Option<HashSet<Uuid>>, DomainError> {
        self.visible_tenants(ctx, resource, action::READ, id).await
    }

    /// The caller's tenant chain, root first, ending at the caller's own
    /// tenant. Used for ancestor visibility and for the Data Plane's walk.
    ///
    /// Fails soft: when the resolver is unavailable the chain is just the
    /// caller's own tenant, so a management request still works.
    async fn tenant_chain(&self, ctx: &SecurityContext) -> Vec<Uuid> {
        let tenant = ctx.subject_tenant_id();
        if tenant.is_nil() {
            return Vec::new();
        }
        let Some(resolver) = &self.tenants else {
            return vec![tenant];
        };
        match resolver
            .get_ancestors(
                ctx,
                tenant_resolver_sdk::TenantId(tenant),
                &tenant_resolver_sdk::models::GetAncestorsOptions {
                    barrier_mode: BarrierMode::Respect,
                },
            )
            .await
        {
            Ok(resp) => {
                let mut chain: Vec<Uuid> = resp.ancestors.iter().map(|t| t.id.0).collect();
                chain.push(tenant);
                chain
            }
            Err(err) => {
                tracing::warn!(%err, %tenant, "tenant ancestor lookup failed; scoping to own tenant");
                vec![tenant]
            }
        }
    }

    // ── Upstreams ───────────────────────────────────────────────────────

    /// Create an upstream.
    ///
    /// # Errors
    ///
    /// [`DomainError::Validation`] on a bad payload,
    /// [`DomainError::Conflict`] on an alias collision, [`DomainError::Forbidden`]
    /// when the PDP denies the write.
    pub async fn create_upstream(
        &self,
        ctx: &SecurityContext,
        payload: Upstream,
    ) -> Result<Upstream, DomainError> {
        let tenant = self
            .check_write(ctx, &UPSTREAM_RESOURCE, action::CREATE, None)
            .await?;
        let now = SystemTime::now();
        let alias = alias::resolve_alias(payload.alias.as_deref(), &payload.server.endpoints)?;
        let mut upstream = payload;
        upstream.id = Some(Uuid::new_v4());
        upstream.alias = Some(alias);
        dto::validate_upstream(&upstream)?;
        dto::validate_upstream_ssrf(&upstream, &self.config.ssrf_policy)?;
        self.validate_upstream_plugins(&upstream).await?;

        let record = UpstreamRecord {
            tenant_id: tenant,
            upstream: upstream.clone(),
            created_at: now,
            updated_at: now,
        };
        self.repos.upstreams.insert(record).await?;
        Ok(upstream)
    }

    /// Fetch one upstream.
    ///
    /// # Errors
    ///
    /// [`DomainError::NotFound`] when the caller's tenant chain has no such
    /// upstream.
    pub async fn get_upstream(
        &self,
        ctx: &SecurityContext,
        id: Uuid,
    ) -> Result<Upstream, DomainError> {
        let allowed = self.check_read(ctx, &UPSTREAM_RESOURCE, Some(id)).await?;
        let chain = self.tenant_chain(ctx).await;
        for tenant in chain {
            if let Some(rec) = self.repos.upstreams.get(tenant, id).await {
                if allowed.is_some_and(|set| !set.contains(&rec.tenant_id)) {
                    return Err(DomainError::Forbidden);
                }
                return Ok(rec.upstream);
            }
        }
        Err(DomainError::upstream_not_found(id))
    }

    /// List upstreams visible to the caller: own tenant plus every ancestor
    /// that shared one.
    ///
    /// # Errors
    ///
    /// [`DomainError::Forbidden`] when the PDP denies.
    pub async fn list_upstreams(
        &self,
        ctx: &SecurityContext,
    ) -> Result<Vec<Upstream>, DomainError> {
        let allowed = self.check_read(ctx, &UPSTREAM_RESOURCE, None).await?;
        let mut out: Vec<Upstream> = Vec::new();
        for tenant in self.tenant_chain(ctx).await {
            for rec in self.repos.upstreams.list(tenant).await {
                if let Some(set) = &allowed
                    && !set.contains(&rec.tenant_id)
                {
                    continue;
                }
                if !out.iter().any(|u| u.id == rec.upstream.id) {
                    out.push(rec.upstream);
                }
            }
        }
        Ok(out)
    }

    /// Replace an upstream. The alias is immutable; `upstream_id` on a route is
    /// likewise, but this is the upstream path.
    ///
    /// # Errors
    ///
    /// [`DomainError::Validation`] when the endpoints would move the alias, or
    /// the payload is otherwise invalid.
    pub async fn replace_upstream(
        &self,
        ctx: &SecurityContext,
        id: Uuid,
        payload: Upstream,
    ) -> Result<Upstream, DomainError> {
        let existing = self.get_upstream_for_write(ctx, id).await?;
        let tenant = existing.tenant_id;

        // Alias immutability, per DESIGN's alias update matrix.
        let was_derivable = alias::derive(&existing.upstream.server.endpoints).is_derived();
        let now_derivation = alias::derive(&payload.server.endpoints);
        if was_derivable && now_derivation.is_requires_explicit() {
            return Err(DomainError::invalid(
                "alias",
                "the endpoints no longer derive an alias (hostname-based endpoints may not become non-derivable); delete and re-create the upstream",
            ));
        }
        let candidate = alias::resolve_alias(
            payload
                .alias
                .as_deref()
                .or(existing.upstream.alias.as_deref()),
            &payload.server.endpoints,
        )?;
        if candidate != existing.alias() {
            return Err(DomainError::invalid(
                "alias",
                format!(
                    "'{}' is immutable once set; delete and re-create the upstream to use '{}'",
                    existing.alias(),
                    candidate
                ),
            ));
        }

        let mut upstream = payload;
        upstream.id = Some(id);
        upstream.alias = Some(candidate);
        dto::validate_upstream(&upstream)?;
        dto::validate_upstream_ssrf(&upstream, &self.config.ssrf_policy)?;
        self.validate_upstream_plugins(&upstream).await?;

        let record = UpstreamRecord {
            tenant_id: tenant,
            upstream: upstream.clone(),
            created_at: existing.created_at,
            updated_at: SystemTime::now(),
        };
        self.repos.upstreams.update(record).await?;
        Ok(upstream)
    }

    /// Delete an upstream. Routes pointing at it are removed with it, so a
    /// delete never leaves a dangling reference behind.
    ///
    /// # Errors
    ///
    /// [`DomainError::NotFound`] when the caller cannot see the upstream.
    pub async fn delete_upstream(
        &self,
        ctx: &SecurityContext,
        id: Uuid,
    ) -> Result<(), DomainError> {
        let existing = self.get_upstream_for_write(ctx, id).await?;
        for route in self.repos.routes.list_for_upstream(id).await {
            self.repos
                .routes
                .delete(existing.tenant_id, route.id())
                .await?;
        }
        self.repos.upstreams.delete(existing.tenant_id, id).await
    }

    async fn get_upstream_for_write(
        &self,
        ctx: &SecurityContext,
        id: Uuid,
    ) -> Result<UpstreamRecord, DomainError> {
        let tenant = self
            .check_write(ctx, &UPSTREAM_RESOURCE, action::OVERRIDE, Some(id))
            .await?;
        self.repos
            .upstreams
            .get(tenant, id)
            .await
            .ok_or_else(|| DomainError::upstream_not_found(id))
    }

    /// Resolve a builtin plugin binding, rejecting an unknown type.
    async fn validate_upstream_plugins(&self, upstream: &Upstream) -> Result<(), DomainError> {
        if let Some(auth) = &upstream.auth
            && let Some(plugin_type) = &auth.plugin_type
        {
            self.registry
                .resolve_auth(plugin_type)
                .map_err(|e| DomainError::invalid("auth.type", e.to_string()))?;
        }
        self.validate_plugin_chain(&upstream.plugins, "plugins.items")
            .await
    }

    async fn validate_plugin_chain(
        &self,
        config: &dto::PluginsConfig,
        field: &str,
    ) -> Result<(), DomainError> {
        for (i, item) in config.items.iter().enumerate() {
            let path = format!("{field}[{i}]");
            match item {
                dto::PluginItem::Reference(id) => {
                    // A GTS identifier for a builtin, or a UUID for a stored
                    // custom plugin. Both must exist.
                    if self.registry.is_builtin(id) {
                        continue;
                    }
                    match crate::gts::uuid_of(id).or_else(|| Uuid::parse_str(id).ok()) {
                        Some(uuid) => {
                            if !self.stored_plugin_exists(uuid).await {
                                return Err(DomainError::invalid(
                                    path,
                                    format!("plugin '{id}' does not exist"),
                                ));
                            }
                        }
                        None => {
                            return Err(DomainError::invalid(
                                path,
                                format!(
                                    "'{id}' is neither a builtin plugin nor a registered plugin id"
                                ),
                            ));
                        }
                    }
                }
                dto::PluginItem::Inline(inline) => {
                    if let Some(t) = &inline.plugin_type
                        && self.registry.resolve_guard(t).is_err()
                        && self.registry.resolve_transform(t).is_err()
                    {
                        return Err(DomainError::invalid(
                            &path,
                            format!("'{t}' is neither a builtin guard nor a transform plugin"),
                        ));
                    }
                }
            }
        }
        Ok(())
    }

    async fn stored_plugin_exists(&self, id: Uuid) -> bool {
        // Ownership is enforced when the binding is actually resolved; here the
        // probe only has to prove the id is live somewhere.
        self.repos.plugins.get_any(id).await.is_some()
    }

    // ── Routes ──────────────────────────────────────────────────────────

    /// Create a route.
    ///
    /// # Errors
    ///
    /// [`DomainError::Validation`] on a bad payload or a match collision,
    /// [`DomainError::NotFound`] when the upstream is unknown.
    pub async fn create_route(
        &self,
        ctx: &SecurityContext,
        payload: Route,
    ) -> Result<Route, DomainError> {
        let tenant = self
            .check_write(ctx, &ROUTE_RESOURCE, action::CREATE, None)
            .await?;
        let mut route = payload;
        route.id = Some(Uuid::new_v4());
        dto::validate_route(&route)?;
        self.validate_plugin_chain(&route.plugins, "plugins.items")
            .await?;

        // The upstream must exist and belong to the caller's tenant chain.
        self.upstream_exists_for(tenant, route.upstream_id).await?;

        self.assert_route_unique(tenant, &route, None).await?;

        let now = SystemTime::now();
        let record = RouteRecord {
            tenant_id: tenant,
            route: route.clone(),
            created_at: now,
            updated_at: now,
        };
        self.repos.routes.insert(record).await?;
        Ok(route)
    }

    /// Fetch one route.
    ///
    /// # Errors
    ///
    /// [`DomainError::NotFound`]
    pub async fn get_route(&self, ctx: &SecurityContext, id: Uuid) -> Result<Route, DomainError> {
        let allowed = self.check_read(ctx, &ROUTE_RESOURCE, Some(id)).await?;
        for tenant in self.tenant_chain(ctx).await {
            if let Some(rec) = self.repos.routes.get(tenant, id).await {
                if allowed.is_some_and(|set| !set.contains(&rec.tenant_id)) {
                    return Err(DomainError::Forbidden);
                }
                return Ok(rec.route);
            }
        }
        Err(DomainError::route_not_found(id))
    }

    /// List routes visible to the caller.
    ///
    /// # Errors
    ///
    /// [`DomainError::Forbidden`]
    pub async fn list_routes(&self, ctx: &SecurityContext) -> Result<Vec<Route>, DomainError> {
        let allowed = self.check_read(ctx, &ROUTE_RESOURCE, None).await?;
        let mut out: Vec<Route> = Vec::new();
        for tenant in self.tenant_chain(ctx).await {
            for rec in self.repos.routes.list(tenant).await {
                if let Some(set) = &allowed
                    && !set.contains(&rec.tenant_id)
                {
                    continue;
                }
                if !out.iter().any(|r| r.id == rec.route.id) {
                    out.push(rec.route);
                }
            }
        }
        Ok(out)
    }

    /// Replace a route. `upstream_id` is immutable (DESIGN).
    ///
    /// # Errors
    ///
    /// [`DomainError::Validation`]
    pub async fn replace_route(
        &self,
        ctx: &SecurityContext,
        id: Uuid,
        payload: Route,
    ) -> Result<Route, DomainError> {
        let tenant = self
            .check_write(ctx, &ROUTE_RESOURCE, action::OVERRIDE, Some(id))
            .await?;
        let existing = self
            .repos
            .routes
            .get(tenant, id)
            .await
            .ok_or_else(|| DomainError::route_not_found(id))?;

        let mut route = payload;
        route.id = Some(id);
        if route.upstream_id != existing.route.upstream_id {
            return Err(DomainError::invalid(
                "upstream_id",
                "upstream_id is immutable on a route; create a new route instead",
            ));
        }
        dto::validate_route(&route)?;
        self.validate_plugin_chain(&route.plugins, "plugins.items")
            .await?;
        self.assert_route_unique(tenant, &route, Some(id)).await?;

        let record = RouteRecord {
            tenant_id: tenant,
            route: route.clone(),
            created_at: existing.created_at,
            updated_at: SystemTime::now(),
        };
        self.repos.routes.update(record).await?;
        Ok(route)
    }

    /// Delete a route.
    ///
    /// # Errors
    ///
    /// [`DomainError::NotFound`]
    pub async fn delete_route(&self, ctx: &SecurityContext, id: Uuid) -> Result<(), DomainError> {
        let tenant = self
            .check_write(ctx, &ROUTE_RESOURCE, action::DELETE, Some(id))
            .await?;
        self.repos.routes.delete(tenant, id).await
    }

    /// No two *enabled* routes under the same upstream may claim the same
    /// `(path, priority)` for an overlapping method.
    async fn assert_route_unique(
        &self,
        tenant: Uuid,
        candidate: &Route,
        except_id: Option<Uuid>,
    ) -> Result<(), DomainError> {
        let Some(http) = &candidate.r#match.http else {
            return Ok(()); // gRPC matches are not colliding in this model
        };
        for rec in self.repos.routes.list(tenant).await {
            if except_id == Some(rec.id()) || !rec.route.enabled || !candidate.enabled {
                continue;
            }
            if rec.route.upstream_id != candidate.upstream_id {
                continue;
            }
            let Some(other) = &rec.route.r#match.http else {
                continue;
            };
            if other.path != http.path || rec.route.priority != candidate.priority {
                continue;
            }
            let clash = http.methods.iter().any(|m| other.methods.contains(m));
            if clash {
                return Err(DomainError::Conflict {
                    kind: "route",
                    message: format!(
                        "a route already matches {} {} with priority {} on this upstream",
                        http.methods
                            .iter()
                            .filter(|m| other.methods.contains(m))
                            .map(std::string::ToString::to_string)
                            .collect::<Vec<_>>()
                            .join("/"),
                        http.path,
                        candidate.priority
                    ),
                });
            }
        }
        Ok(())
    }

    async fn upstream_exists_for(
        &self,
        tenant: Uuid,
        upstream_id: Uuid,
    ) -> Result<(), DomainError> {
        for candidate in self.tenant_chain_ids(tenant).await {
            if self
                .repos
                .upstreams
                .get(candidate, upstream_id)
                .await
                .is_some()
            {
                return Ok(());
            }
        }
        Err(DomainError::upstream_not_found(upstream_id))
    }

    /// Ancestor chain of a tenant id, computed from stored tenant ids alone.
    async fn tenant_chain_ids(&self, tenant: Uuid) -> Vec<Uuid> {
        vec![tenant]
    }

    // ── Plugins ─────────────────────────────────────────────────────────

    /// Create a custom plugin. Plugins are immutable once created.
    ///
    /// # Errors
    ///
    /// [`DomainError::Validation`]
    pub async fn create_plugin(
        &self,
        ctx: &SecurityContext,
        payload: Plugin,
    ) -> Result<Plugin, DomainError> {
        let tenant = self
            .check_write(ctx, plugin_resource(payload.kind), action::CREATE, None)
            .await?;
        let mut plugin = payload;
        plugin.id = Some(Uuid::new_v4());
        dto::validate_plugin(&plugin)?;

        let record = PluginRecord {
            tenant_id: tenant,
            plugin: plugin.clone(),
            gc_eligible_at: None,
            created_at: SystemTime::now(),
        };
        self.repos.plugins.insert(record).await?;
        Ok(plugin)
    }

    /// Fetch one custom plugin.
    ///
    /// # Errors
    ///
    /// [`DomainError::NotFound`]
    pub async fn get_plugin(&self, ctx: &SecurityContext, id: Uuid) -> Result<Plugin, DomainError> {
        let mut found: Option<PluginRecord> = None;
        for tenant in self.tenant_chain(ctx).await {
            if let Some(rec) = self.repos.plugins.get(tenant, id).await {
                found = Some(rec);
                break;
            }
        }
        let rec = found.ok_or_else(|| DomainError::plugin_not_found(id))?;
        let allowed = self
            .check_read(ctx, plugin_resource(rec.plugin.kind), Some(id))
            .await?;
        if let Some(set) = allowed
            && !set.contains(&rec.tenant_id)
        {
            return Err(DomainError::Forbidden);
        }
        Ok(rec.plugin)
    }

    /// List custom plugins visible to the caller.
    ///
    /// # Errors
    ///
    /// [`DomainError::Forbidden`]
    pub async fn list_plugins(
        &self,
        ctx: &SecurityContext,
        kind: Option<PluginKind>,
    ) -> Result<Vec<Plugin>, DomainError> {
        let allowed = self
            .check_read(
                ctx,
                plugin_resource(kind.unwrap_or(PluginKind::Transform)),
                None,
            )
            .await?;
        let mut out: Vec<Plugin> = Vec::new();
        for tenant in self.tenant_chain(ctx).await {
            for rec in self.repos.plugins.list(tenant).await {
                if let Some(k) = kind
                    && rec.plugin.kind != k
                {
                    continue;
                }
                if let Some(set) = &allowed
                    && !set.contains(&rec.tenant_id)
                {
                    continue;
                }
                if !out.iter().any(|p| p.id == rec.plugin.id) {
                    out.push(rec.plugin);
                }
            }
        }
        Ok(out)
    }

    /// Custom plugins are immutable: an update attempt is a conflict.
    ///
    /// # Errors
    ///
    /// Always [`DomainError::Conflict`].
    pub fn update_plugin(&self) -> Result<Plugin, DomainError> {
        Err(DomainError::Conflict {
            kind: "plugin",
            message: "custom plugins are immutable; create a new plugin and update the references"
                .to_owned(),
        })
    }

    /// Delete an unreferenced plugin.
    ///
    /// # Errors
    ///
    /// [`DomainError::PluginInUse`] when an upstream or route still binds it.
    pub async fn delete_plugin(&self, ctx: &SecurityContext, id: Uuid) -> Result<(), DomainError> {
        let tenant = self.own_tenant(ctx).await?;
        let stored = self.repos.plugins.get(tenant, id).await;
        if stored.is_none() {
            // Ancestor-owned plugins are not deletable by descendants.
            for ancestor in self.tenant_chain(ctx).await {
                if self.repos.plugins.get(ancestor, id).await.is_some() {
                    return Err(DomainError::Forbidden);
                }
            }
            return Err(DomainError::plugin_not_found(id));
        }
        let mut referenced_by = crate::domain::error::ReferencedBy::default();
        for up in self.repos.upstreams.referencing_plugin(id).await {
            referenced_by.upstreams.push(up.gts_id());
        }
        for rt in self.repos.routes.referencing_plugin(id).await {
            referenced_by.routes.push(rt.gts_id());
        }
        if !referenced_by.is_empty() {
            // The identifier names the plugin under its own resource type: a
            // guard a caller may not delete should not be reported as an auth
            // plugin, which is a different GTS type entirely.
            return Err(DomainError::PluginInUse {
                plugin_id: stored.map_or_else(
                    || crate::gts::instance_id(crate::gts::AUTH_PLUGIN_TYPE, id),
                    |r| r.gts_id(),
                ),
                referenced_by,
            });
        }
        self.repos.plugins.delete(tenant, id).await
    }

    /// The Starlark source of a plugin. Never logged; returned verbatim.
    ///
    /// # Errors
    ///
    /// [`DomainError::NotFound`]
    pub async fn plugin_source(
        &self,
        ctx: &SecurityContext,
        id: Uuid,
    ) -> Result<String, DomainError> {
        let plugin = self.get_plugin(ctx, id).await?;
        plugin
            .source
            .ok_or_else(|| DomainError::plugin_not_found(id))
    }

    /// Mark unreferenced plugins eligible for collection.
    ///
    /// Called by the background GC task and after any write that could change
    /// the reference graph.
    pub async fn refresh_gc_marks(&self, ttl: std::time::Duration) -> usize {
        let now = SystemTime::now();
        let deadline = now + ttl;
        let mut marked = 0usize;
        let all = self.all_plugin_ids().await;
        for (tenant, id) in all {
            let in_use = !self.repos.upstreams.referencing_plugin(id).await.is_empty()
                || !self.repos.routes.referencing_plugin(id).await.is_empty();
            let Some(mut rec) = self.repos.plugins.get(tenant, id).await else {
                continue;
            };
            if in_use {
                if rec.gc_eligible_at.is_some() {
                    let _ = self.repos.plugins.unmark_for_gc(tenant, id).await;
                }
            } else if rec.gc_eligible_at.is_none() {
                rec.gc_eligible_at = Some(deadline);
                let _ = self.repos.plugins.update(rec).await;
                marked += 1;
            }
        }
        marked
    }

    /// Delete plugins whose eligibility deadline has passed.
    pub async fn collect_plugins(&self) -> usize {
        let mut removed = 0usize;
        for rec in self.repos.plugins.collectible(SystemTime::now()).await {
            if self
                .repos
                .plugins
                .delete(rec.tenant_id, rec.id())
                .await
                .is_ok()
            {
                removed += 1;
                tracing::info!(plugin_id = %rec.id(), "collected unreferenced oagw plugin");
            }
        }
        removed
    }

    async fn all_plugin_ids(&self) -> Vec<(Uuid, Uuid)> {
        // The repository is tenant-scoped; sweep the tenants we know about
        // through the callers' chain at write time. For GC we sweep every
        // plugin the store holds by asking for each tenant we have seen.
        self.repos.plugins.all_tenants().await
    }

    /// A no-op kept for symmetry with the other `check_*` helpers.
    async fn check_proxy_scope(&self, ctx: &SecurityContext) -> Result<(), DomainError> {
        if let Some(allowed) = self
            .visible_tenants(ctx, &PROXY_RESOURCE, action::INVOKE, None)
            .await?
            && allowed.is_empty()
        {
            return Err(DomainError::Forbidden);
        }
        Ok(())
    }

    /// Whether the caller may proxy at all.
    ///
    /// # Errors
    ///
    /// [`DomainError::Forbidden`]
    pub async fn authorize_proxy(&self, ctx: &SecurityContext) -> Result<(), DomainError> {
        self.check_proxy_scope(ctx).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::infra::plugin::test_support;
    use crate::infra::storage::memory;

    fn service(store: &memory::Stores) -> ManagementService {
        ManagementService::new(
            memory::repos(store),
            None,
            None,
            PluginRegistry::with_builtins(None, crate::config::TokenCacheConfig::default()),
            OagwConfig {
                ssrf_policy: crate::config::SsrfPolicy {
                    enabled: false,
                    ..OagwConfig::default().ssrf_policy
                },
                ..OagwConfig::default()
            },
        )
    }

    fn plugin(kind: PluginKind) -> Plugin {
        Plugin {
            id: None,
            kind,
            name: Some("p".to_owned()),
            description: None,
            tags: Vec::new(),
            config: None,
            source: Some("def guard_request(ctx):\n    return None\n".to_owned()),
        }
    }

    #[tokio::test]
    async fn a_plugin_in_use_is_named_under_its_own_resource_type() {
        let store = memory::Stores::default();
        let svc = service(&store);
        let ctx = test_support::security_context();

        let guard = svc
            .create_plugin(&ctx, plugin(PluginKind::Guard))
            .await
            .expect("the fixture plugin must create");
        let id = guard.id.expect("create_plugin assigns an id");

        let mut upstream = Upstream {
            id: Some(Uuid::new_v4()),
            enabled: true,
            alias: Some("bound.test".to_owned()),
            tags: Vec::new(),
            server: crate::domain::dto::ServerConfig {
                endpoints: vec![crate::domain::dto::Endpoint {
                    scheme: crate::domain::dto::Scheme::Http,
                    host: "127.0.0.1".to_owned(),
                    port: Some(9_099),
                }],
            },
            protocol: crate::domain::dto::Protocol::Http,
            auth: None,
            headers: crate::domain::dto::HeadersConfig::default(),
            plugins: crate::domain::dto::PluginsConfig::default(),
            rate_limit: None,
            cors: None,
        };
        upstream
            .plugins
            .items
            .push(crate::domain::dto::PluginItem::Reference(id.to_string()));
        svc.create_upstream(&ctx, upstream).await.expect("upstream");

        let err = svc.delete_plugin(&ctx, id).await.expect_err("referenced");
        let DomainError::PluginInUse {
            plugin_id,
            referenced_by,
        } = err
        else {
            panic!("expected PluginInUse");
        };
        // A guard is a `guard_plugin`, not the `auth_plugin` the old
        // construction reported; the type is what an operator matches on.
        assert_eq!(
            plugin_id,
            format!("gts.cf.core.oagw.guard_plugin.v1~{}", id.simple())
        );
        assert_eq!(referenced_by.upstreams.len(), 1);
    }
}
