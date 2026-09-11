//! `ControlPlaneService` — configuration ownership.
//!
//! Everything that reads or writes upstreams, routes and plugins goes through
//! here (`cpt-cf-oagw-adr-request-routing`). Two rules dominate the code:
//!
//! * **Tenant scoping** — every CRUD operation is scoped to the calling
//!   tenant, and an ancestor's resources are invisible (`404`) through the
//!   management API (`DESIGN.md` § *Tenant Scoping*).
//! * **Validation before persistence** — an upstream or route that would be
//!   unroutable is refused at write time, so the proxy path never has to
//!   defend against malformed configuration.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::domain::alias::{enforce_alias_update, resolve_alias_for_create, validate_host};
use crate::domain::dto::{PluginSpec, RouteSpec, UpstreamSpec};
use crate::domain::error::{ErrorKind, OagwError, OagwResult};
use crate::domain::gts_helpers::{self, PluginKind, ROUTE_TYPE, UPSTREAM_TYPE, plugin_ref_uuid};
use crate::domain::model::{
    HttpMatch, MatchConfig, PluginRecord, PluginsConfig, Protocol, Route, SharingMode, Upstream,
};
use crate::domain::plugin::PluginCatalog;
use crate::domain::repo::{
    PluginRepository, PluginUsage, PluginUsageRepository, RouteRepository, UpstreamRepository,
};
use crate::domain::tenant::TenantChain;

/// Methods the HTTP match rule may name.
const ALLOWED_METHODS: &[&str] = &["GET", "POST", "PUT", "DELETE", "PATCH"];
/// Upper bound on the endpoint pool, so one upstream cannot exhaust the
/// connector's per-host budget.
const MAX_ENDPOINTS: usize = 32;
/// Upper bound on a plugin chain.
const MAX_PLUGIN_CHAIN: usize = 32;

/// The Control Plane.
pub struct ControlPlane {
    upstreams: Arc<dyn UpstreamRepository>,
    routes: Arc<dyn RouteRepository>,
    plugins: Arc<dyn PluginRepository>,
    usage: Arc<dyn PluginUsageRepository>,
    tenants: Arc<dyn TenantChain>,
    catalog: Arc<dyn PluginCatalog>,
    seq: AtomicU64,
    /// GC grace period, expressed in store-clock ticks.
    gc_grace: u64,
}

impl ControlPlane {
    /// Wire the Control Plane to its repositories and collaborators.
    #[must_use]
    pub fn new(
        upstreams: Arc<dyn UpstreamRepository>,
        routes: Arc<dyn RouteRepository>,
        plugins: Arc<dyn PluginRepository>,
        usage: Arc<dyn PluginUsageRepository>,
        tenants: Arc<dyn TenantChain>,
        catalog: Arc<dyn PluginCatalog>,
        gc_grace: u64,
    ) -> Self {
        Self {
            upstreams,
            routes,
            plugins,
            usage,
            tenants,
            catalog,
            seq: AtomicU64::new(0),
            gc_grace,
        }
    }

    /// The tenant-hierarchy view, for the Data Plane's resolution walk.
    #[must_use]
    pub fn tenants(&self) -> &Arc<dyn TenantChain> {
        &self.tenants
    }

    /// The upstream repository, for the Data Plane's resolution walk.
    #[must_use]
    pub fn upstream_repo(&self) -> &Arc<dyn UpstreamRepository> {
        &self.upstreams
    }

    /// The route repository, for the Data Plane's resolution walk.
    #[must_use]
    pub fn route_repo(&self) -> &Arc<dyn RouteRepository> {
        &self.routes
    }

    /// The custom-plugin repository, for resolving UUID-backed bindings.
    #[must_use]
    pub fn plugin_repo(&self) -> &Arc<dyn PluginRepository> {
        &self.plugins
    }

    fn next_seq(&self) -> u64 {
        self.seq.fetch_add(1, Ordering::Relaxed)
    }

    // ---------------------------------------------------------------- upstreams

    /// Create an upstream for the calling tenant.
    ///
    /// # Errors
    ///
    /// `400` on validation failure, `409` when `(tenant_id, alias)` is taken.
    pub async fn create_upstream(
        &self,
        ctx: &SecurityContext,
        spec: UpstreamSpec,
    ) -> OagwResult<Upstream> {
        let tenant_id = ctx.subject_tenant_id();
        self.validate_upstream_spec(&spec).await?;
        let alias = resolve_alias_for_create(&spec.server.endpoints, spec.alias.as_deref())?;
        self.check_ancestor_bind(ctx, tenant_id, &alias, &spec)
            .await?;

        let upstream = Upstream {
            id: Uuid::new_v4(),
            tenant_id,
            alias,
            enabled: spec.enabled.unwrap_or(true),
            protocol: spec.protocol,
            server: spec.server,
            auth: spec.auth,
            headers: spec.headers,
            plugins: spec.plugins,
            rate_limit: spec.rate_limit,
            cors: spec.cors,
            tags: normalize_tags(spec.tags.unwrap_or_default())?,
            seq: self.next_seq(),
        };
        let created = self.upstreams.create(upstream).await?;
        tracing::info!(
            target: "oagw.audit",
            event = "upstream_created",
            tenant_id = %created.tenant_id,
            upstream_id = %created.id,
            alias = %created.alias,
            "upstream configuration created"
        );
        Ok(created)
    }

    /// Fetch an upstream owned by the calling tenant.
    ///
    /// # Errors
    ///
    /// `404` when the upstream does not exist for this tenant.
    pub async fn get_upstream(&self, ctx: &SecurityContext, id: Uuid) -> OagwResult<Upstream> {
        self.upstreams
            .get(ctx.subject_tenant_id(), id)
            .await?
            .ok_or_else(|| upstream_not_found(id))
    }

    /// Replace an upstream wholesale.
    ///
    /// # Errors
    ///
    /// `400` on validation failure (including an alias-changing endpoint
    /// change), `404` when the upstream is not visible to this tenant.
    pub async fn replace_upstream(
        &self,
        ctx: &SecurityContext,
        id: Uuid,
        spec: UpstreamSpec,
    ) -> OagwResult<Upstream> {
        let tenant_id = ctx.subject_tenant_id();
        let existing = self
            .upstreams
            .get(tenant_id, id)
            .await?
            .ok_or_else(|| upstream_not_found(id))?;
        self.validate_upstream_spec(&spec).await?;
        let alias = enforce_alias_update(
            &existing.alias,
            &spec.server.endpoints,
            spec.alias.as_deref(),
        )?;
        self.check_ancestor_bind(ctx, tenant_id, &alias, &spec)
            .await?;

        let replaced = Upstream {
            id: existing.id,
            tenant_id: existing.tenant_id,
            alias,
            enabled: spec.enabled.unwrap_or(true),
            protocol: spec.protocol,
            server: spec.server,
            auth: spec.auth,
            headers: spec.headers,
            plugins: spec.plugins,
            rate_limit: spec.rate_limit,
            cors: spec.cors,
            tags: normalize_tags(spec.tags.unwrap_or_default())?,
            seq: existing.seq,
        };
        let replaced = self.upstreams.replace(replaced).await?;
        self.reconcile_plugin_gc(tenant_id).await;
        tracing::info!(
            target: "oagw.audit",
            event = "upstream_replaced",
            tenant_id = %replaced.tenant_id,
            upstream_id = %replaced.id,
            "upstream configuration replaced"
        );
        Ok(replaced)
    }

    /// Delete an upstream and cascade to its routes.
    ///
    /// # Errors
    ///
    /// `404` when the upstream is not visible to this tenant.
    pub async fn delete_upstream(&self, ctx: &SecurityContext, id: Uuid) -> OagwResult<()> {
        let tenant_id = ctx.subject_tenant_id();
        if !self.upstreams.delete(tenant_id, id).await? {
            return Err(upstream_not_found(id));
        }
        self.reconcile_plugin_gc(tenant_id).await;
        tracing::info!(
            target: "oagw.audit",
            event = "upstream_deleted",
            tenant_id = %tenant_id,
            upstream_id = %id,
            "upstream configuration deleted"
        );
        Ok(())
    }

    /// All upstreams owned by the calling tenant.
    ///
    /// # Errors
    ///
    /// Storage failures.
    pub async fn list_upstreams(&self, ctx: &SecurityContext) -> OagwResult<Vec<Upstream>> {
        self.upstreams.list(ctx.subject_tenant_id()).await
    }

    // ------------------------------------------------------------------- routes

    /// Create a route on an upstream owned by the calling tenant.
    ///
    /// # Errors
    ///
    /// `400` when the upstream reference or the match rule is invalid, `409`
    /// when an equivalent match rule already exists.
    pub async fn create_route(&self, ctx: &SecurityContext, spec: RouteSpec) -> OagwResult<Route> {
        let tenant_id = ctx.subject_tenant_id();
        let raw = spec.upstream_id.as_deref().ok_or_else(|| {
            OagwError::field(
                "upstream_id",
                "upstream_id is required when creating a route",
            )
        })?;
        let upstream_id = gts_helpers::parse_resource_id(UPSTREAM_TYPE, raw).ok_or_else(|| {
            OagwError::field(
                "upstream_id",
                format!("upstream_id must be a UUID or {UPSTREAM_TYPE}{{uuid}}: {raw:?}"),
            )
        })?;
        // Ancestor upstreams are not directly addressable: a route may only
        // hang off an upstream this tenant owns.
        let upstream = self
            .upstreams
            .get(tenant_id, upstream_id)
            .await?
            .ok_or_else(|| {
                OagwError::field(
                    "upstream_id",
                    format!("upstream {upstream_id} not found for this tenant"),
                )
            })?;

        let match_config = self.validate_match(&spec.match_config, upstream.protocol)?;
        self.validate_route_extras(&spec).await?;
        self.ensure_match_is_unique(upstream_id, &match_config, spec.priority.unwrap_or(0), None)
            .await?;

        let route = Route {
            id: Uuid::new_v4(),
            tenant_id,
            upstream_id,
            enabled: spec.enabled.unwrap_or(true),
            priority: spec.priority.unwrap_or(0),
            match_config,
            rate_limit: spec.rate_limit,
            cors: spec.cors,
            plugins: spec.plugins,
            tags: normalize_tags(spec.tags.unwrap_or_default())?,
            seq: self.next_seq(),
        };
        let created = self.routes.create(route).await?;
        tracing::info!(
            target: "oagw.audit",
            event = "route_created",
            tenant_id = %created.tenant_id,
            route_id = %created.id,
            upstream_id = %created.upstream_id,
            "route configuration created"
        );
        Ok(created)
    }

    /// Fetch a route owned by the calling tenant.
    ///
    /// # Errors
    ///
    /// `404` when the route does not exist for this tenant.
    pub async fn get_route(&self, ctx: &SecurityContext, id: Uuid) -> OagwResult<Route> {
        self.routes
            .get(ctx.subject_tenant_id(), id)
            .await?
            .ok_or_else(|| route_not_found(id))
    }

    /// Replace a route wholesale. `upstream_id` is immutable.
    ///
    /// # Errors
    ///
    /// `400` on validation failure or an attempted `upstream_id` change,
    /// `404` when the route is not visible, `409` on match-rule collision.
    pub async fn replace_route(
        &self,
        ctx: &SecurityContext,
        id: Uuid,
        spec: RouteSpec,
    ) -> OagwResult<Route> {
        let tenant_id = ctx.subject_tenant_id();
        let existing = self
            .routes
            .get(tenant_id, id)
            .await?
            .ok_or_else(|| route_not_found(id))?;

        if let Some(raw) = spec.upstream_id.as_deref() {
            let requested =
                gts_helpers::parse_resource_id(UPSTREAM_TYPE, raw).ok_or_else(|| {
                    OagwError::field(
                        "upstream_id",
                        format!("upstream_id must be a UUID or {UPSTREAM_TYPE}{{uuid}}: {raw:?}"),
                    )
                })?;
            if requested != existing.upstream_id {
                return Err(OagwError::field(
                    "upstream_id",
                    "upstream_id is immutable; delete and re-create the route instead",
                ));
            }
        }

        let upstream = self
            .upstreams
            .get(tenant_id, existing.upstream_id)
            .await?
            .ok_or_else(|| route_not_found(id))?;
        let match_config = self.validate_match(&spec.match_config, upstream.protocol)?;
        self.validate_route_extras(&spec).await?;
        self.ensure_match_is_unique(
            existing.upstream_id,
            &match_config,
            spec.priority.unwrap_or(0),
            Some(id),
        )
        .await?;

        let replaced = Route {
            id: existing.id,
            tenant_id: existing.tenant_id,
            upstream_id: existing.upstream_id,
            enabled: spec.enabled.unwrap_or(true),
            priority: spec.priority.unwrap_or(0),
            match_config,
            rate_limit: spec.rate_limit,
            cors: spec.cors,
            plugins: spec.plugins,
            tags: normalize_tags(spec.tags.unwrap_or_default())?,
            seq: existing.seq,
        };
        let replaced = self.routes.replace(replaced).await?;
        self.reconcile_plugin_gc(tenant_id).await;
        Ok(replaced)
    }

    /// Delete a route.
    ///
    /// # Errors
    ///
    /// `404` when the route is not visible to this tenant.
    pub async fn delete_route(&self, ctx: &SecurityContext, id: Uuid) -> OagwResult<()> {
        let tenant_id = ctx.subject_tenant_id();
        if !self.routes.delete(tenant_id, id).await? {
            return Err(route_not_found(id));
        }
        self.reconcile_plugin_gc(tenant_id).await;
        Ok(())
    }

    /// All routes owned by the calling tenant.
    ///
    /// # Errors
    ///
    /// Storage failures.
    pub async fn list_routes(&self, ctx: &SecurityContext) -> OagwResult<Vec<Route>> {
        self.routes.list(ctx.subject_tenant_id()).await
    }

    // ------------------------------------------------------------------ plugins

    /// Create a custom plugin. Plugins are immutable after creation.
    ///
    /// # Errors
    ///
    /// `400` on validation failure, `409` when `(tenant_id, name)` is taken.
    pub async fn create_plugin(
        &self,
        ctx: &SecurityContext,
        spec: PluginSpec,
    ) -> OagwResult<PluginRecord> {
        let tenant_id = ctx.subject_tenant_id();
        let kind = parse_plugin_kind(&spec.plugin_type)?;
        if spec.name.trim().is_empty() {
            return Err(OagwError::field("name", "plugin name must not be empty"));
        }
        if spec.source_code.trim().is_empty() {
            return Err(OagwError::field(
                "source_code",
                "plugin source_code must not be empty",
            ));
        }
        for phase in spec.phases.iter().flatten() {
            if !matches!(phase.as_str(), "on_request" | "on_response" | "on_error") {
                return Err(OagwError::field(
                    "phases",
                    format!("phase must be one of [on_request, on_response, on_error]: {phase:?}"),
                ));
            }
        }
        let record = PluginRecord {
            id: Uuid::new_v4(),
            tenant_id,
            plugin_type: kind,
            name: spec.name.trim().to_owned(),
            description: spec.description,
            phases: spec.phases.unwrap_or_default(),
            config_schema: spec.config_schema,
            source_code: spec.source_code,
            gc_eligible_at: None,
            seq: self.next_seq(),
        };
        self.plugins.create(record).await
    }

    /// Fetch a plugin owned by the calling tenant.
    ///
    /// # Errors
    ///
    /// `404` when the plugin does not exist for this tenant.
    pub async fn get_plugin(&self, ctx: &SecurityContext, id: Uuid) -> OagwResult<PluginRecord> {
        self.plugins
            .get(ctx.subject_tenant_id(), id)
            .await?
            .ok_or_else(|| OagwError::not_found(format!("plugin {id} not found")))
    }

    /// All plugins owned by the calling tenant.
    ///
    /// # Errors
    ///
    /// Storage failures.
    pub async fn list_plugins(&self, ctx: &SecurityContext) -> OagwResult<Vec<PluginRecord>> {
        self.plugins.list(ctx.subject_tenant_id()).await
    }

    /// Delete a plugin. Only unlinked plugins can be deleted.
    ///
    /// # Errors
    ///
    /// `404` when the plugin is not visible, `409 PluginInUse` when it is
    /// still referenced.
    pub async fn delete_plugin(&self, ctx: &SecurityContext, id: Uuid) -> OagwResult<()> {
        let plugin = self.get_plugin(ctx, id).await?;
        let plugin_ref = plugin.plugin_ref();
        let usage = self.usage.references(&plugin_ref).await?;
        if !usage.is_empty() {
            return Err(plugin_in_use(&plugin_ref, &usage));
        }
        self.plugins.delete(ctx.subject_tenant_id(), id).await?;
        Ok(())
    }

    /// Re-stamp the GC deadline on one tenant's plugins after a
    /// configuration write may have linked or unlinked some of them.
    ///
    /// Best-effort maintenance: a storage failure here is logged, never
    /// surfaced, because the write it follows has already succeeded.
    pub async fn reconcile_plugin_gc(&self, tenant_id: Uuid) {
        self.stamp_gc_deadlines(tenant_id).await;
        let now = self.usage.clock();
        if let Err(err) = self.usage.collect_garbage(now).await {
            tracing::warn!(target: "oagw.gc", error = %err, "plugin garbage collection failed");
        }
    }

    /// A plugin that is referenced has no deadline; one that is not gets a
    /// deadline `gc_grace` ticks out, set once and then left alone so the
    /// grace period is not renewed on every write.
    async fn stamp_gc_deadlines(&self, tenant_id: Uuid) {
        let Ok(plugins) = self.plugins.list(tenant_id).await else {
            return;
        };
        let now = self.usage.clock();
        for plugin in plugins {
            let plugin_ref = plugin.plugin_ref();
            let Ok(usage) = self.usage.references(&plugin_ref).await else {
                continue;
            };
            let next = if usage.is_empty() {
                Some(
                    plugin
                        .gc_eligible_at
                        .unwrap_or_else(|| now.saturating_add(self.gc_grace)),
                )
            } else {
                None
            };
            if next != plugin.gc_eligible_at
                && let Err(err) = self.plugins.set_gc_eligible(plugin.id, next).await
            {
                tracing::warn!(
                    target: "oagw.gc",
                    plugin_id = %plugin.id,
                    error = %err,
                    "could not update plugin GC deadline"
                );
            }
        }
    }

    // --------------------------------------------------------------- validation

    /// Validate everything about an upstream write that does not depend on
    /// what is already stored.
    async fn validate_upstream_spec(&self, spec: &UpstreamSpec) -> OagwResult<()> {
        let endpoints = &spec.server.endpoints;
        if endpoints.is_empty() {
            return Err(OagwError::field(
                "server.endpoints",
                "at least one endpoint is required",
            ));
        }
        if endpoints.len() > MAX_ENDPOINTS {
            return Err(OagwError::field(
                "server.endpoints",
                format!("at most {MAX_ENDPOINTS} endpoints are allowed in one pool"),
            ));
        }
        for endpoint in endpoints {
            validate_host(&endpoint.host)?;
        }
        // Pool homogeneity: mixing schemes or ports would make the pool
        // members non-interchangeable.
        let first = &endpoints[0];
        for endpoint in &endpoints[1..] {
            if endpoint.scheme != first.scheme {
                return Err(OagwError::field(
                    "server.endpoints",
                    "all endpoints in a pool must share the same scheme",
                ));
            }
            if endpoint.port() != first.port() {
                return Err(OagwError::field(
                    "server.endpoints",
                    "all endpoints in a pool must share the same port",
                ));
            }
        }

        if let Some(auth) = &spec.auth {
            self.validate_auth_plugin(&auth.plugin_type).await?;
        }
        if let Some(plugins) = &spec.plugins {
            self.validate_plugin_chain(plugins, "plugins").await?;
        }
        if let Some(rate_limit) = &spec.rate_limit {
            rate_limit.validate("rate_limit")?;
        }
        if let Some(cors) = &spec.cors {
            cors.validate("cors")?;
        }
        Ok(())
    }

    async fn validate_route_extras(&self, spec: &RouteSpec) -> OagwResult<()> {
        if let Some(plugins) = &spec.plugins {
            self.validate_plugin_chain(plugins, "plugins").await?;
        }
        if let Some(rate_limit) = &spec.rate_limit {
            rate_limit.validate("rate_limit")?;
        }
        if let Some(cors) = &spec.cors {
            cors.validate("cors")?;
        }
        Ok(())
    }

    /// An auth plugin identifier must resolve to a real implementation.
    ///
    /// `basic.v1` / `bearer.v1` are catalogued but unimplemented, so they land
    /// here as `unknown auth plugin` rather than failing at request time.
    async fn validate_auth_plugin(&self, plugin_type: &str) -> OagwResult<()> {
        if let Some(uuid) = plugin_ref_uuid(plugin_type) {
            let record = self.plugins.get_unscoped(uuid).await?;
            return match record {
                Some(record) if record.plugin_type == PluginKind::Auth => Ok(()),
                Some(_) => Err(OagwError::field(
                    "auth.type",
                    format!("plugin {uuid} is not an auth plugin"),
                )),
                None => Err(OagwError::field(
                    "auth.type",
                    format!("unknown auth plugin: {plugin_type}"),
                )),
            };
        }
        if self.catalog.has_auth(plugin_type) {
            return Ok(());
        }
        Err(OagwError::field(
            "auth.type",
            format!(
                "unknown auth plugin: {plugin_type} (resolvable: {})",
                self.catalog.auth_ids().join(", ")
            ),
        ))
    }

    /// Every chain entry must resolve, and must be a guard or a transform —
    /// an auth plugin belongs in `auth`, not in the chain.
    async fn validate_plugin_chain(&self, plugins: &PluginsConfig, field: &str) -> OagwResult<()> {
        if plugins.items.len() > MAX_PLUGIN_CHAIN {
            return Err(OagwError::field(
                field,
                format!("at most {MAX_PLUGIN_CHAIN} plugins may be bound"),
            ));
        }
        for binding in &plugins.items {
            let plugin_ref = binding.plugin_ref.trim();
            if plugin_ref.is_empty() {
                return Err(OagwError::field(field, "plugin_ref must not be empty"));
            }
            if let Some(uuid) = plugin_ref_uuid(plugin_ref) {
                let record = self.plugins.get_unscoped(uuid).await?.ok_or_else(|| {
                    OagwError::field(field, format!("unknown plugin: {plugin_ref}"))
                })?;
                let declared = PluginKind::from_plugin_ref(plugin_ref).ok_or_else(|| {
                    OagwError::field(
                        field,
                        format!("plugin_ref does not name a plugin type: {plugin_ref}"),
                    )
                })?;
                if declared != record.plugin_type {
                    return Err(OagwError::field(
                        field,
                        format!(
                            "plugin {uuid} is a {} plugin but was bound as {}",
                            record.plugin_type.as_str(),
                            declared.as_str()
                        ),
                    ));
                }
                if record.plugin_type == PluginKind::Auth {
                    return Err(OagwError::field(
                        field,
                        "auth plugins are bound through `auth`, not through `plugins.items`",
                    ));
                }
                continue;
            }
            match PluginKind::from_plugin_ref(plugin_ref) {
                Some(PluginKind::Guard) if self.catalog.has_guard(plugin_ref) => {}
                Some(PluginKind::Transform) if self.catalog.has_transform(plugin_ref) => {}
                Some(PluginKind::Auth) => {
                    return Err(OagwError::field(
                        field,
                        "auth plugins are bound through `auth`, not through `plugins.items`",
                    ));
                }
                _ => {
                    return Err(OagwError::field(
                        field,
                        format!(
                            "unknown or non-bindable plugin: {plugin_ref} (timeout, cors, \
                             logging and metrics are core Data Plane behaviour and cannot be \
                             bound)"
                        ),
                    ));
                }
            }
        }
        Ok(())
    }

    /// Validate the match rule against the upstream's protocol and normalise
    /// it (uppercase methods, leading-slash path).
    fn validate_match(
        &self,
        match_config: &MatchConfig,
        protocol: Protocol,
    ) -> OagwResult<MatchConfig> {
        match (&match_config.http, &match_config.grpc) {
            (Some(_), Some(_)) | (None, None) => {
                return Err(OagwError::field(
                    "match",
                    "exactly one of match.http or match.grpc must be present",
                ));
            }
            _ => {}
        }
        if let Some(http) = &match_config.http {
            if protocol != Protocol::Http {
                return Err(OagwError::field(
                    "match",
                    "match.http requires an upstream with the HTTP protocol",
                ));
            }
            if http.methods.is_empty() {
                return Err(OagwError::field(
                    "match.http.methods",
                    "at least one method is required",
                ));
            }
            let mut methods = Vec::with_capacity(http.methods.len());
            for method in &http.methods {
                let upper = method.trim().to_ascii_uppercase();
                if !ALLOWED_METHODS.contains(&upper.as_str()) {
                    return Err(OagwError::field(
                        "match.http.methods",
                        format!(
                            "method must be one of [{}]: {method:?}",
                            ALLOWED_METHODS.join(", ")
                        ),
                    ));
                }
                if !methods.contains(&upper) {
                    methods.push(upper);
                }
            }
            let path = http.path.trim();
            if path.is_empty() {
                return Err(OagwError::field(
                    "match.http.path",
                    "path must not be empty",
                ));
            }
            if path.contains('?') || path.contains('#') {
                return Err(OagwError::field(
                    "match.http.path",
                    "path must not contain a query string or fragment",
                ));
            }
            let path = if path.starts_with('/') {
                path.to_owned()
            } else {
                format!("/{path}")
            };
            return Ok(MatchConfig {
                http: Some(HttpMatch {
                    methods,
                    path,
                    query_allowlist: http
                        .query_allowlist
                        .iter()
                        .map(|name| name.trim().to_owned())
                        .filter(|name| !name.is_empty())
                        .collect(),
                    path_suffix_mode: http.path_suffix_mode,
                }),
                grpc: None,
            });
        }
        if let Some(grpc) = &match_config.grpc {
            if protocol != Protocol::Grpc {
                return Err(OagwError::field(
                    "match",
                    "match.grpc requires an upstream with the gRPC protocol",
                ));
            }
            if grpc.service.trim().is_empty() || grpc.method.trim().is_empty() {
                return Err(OagwError::field(
                    "match.grpc",
                    "service and method must not be empty",
                ));
            }
        }
        Ok(match_config.clone())
    }

    /// Refuse a second enabled route with the same `(path, priority, method)`
    /// on one upstream — otherwise matching would be non-deterministic.
    async fn ensure_match_is_unique(
        &self,
        upstream_id: Uuid,
        match_config: &MatchConfig,
        priority: i32,
        exclude: Option<Uuid>,
    ) -> OagwResult<()> {
        let Some(http) = &match_config.http else {
            return Ok(());
        };
        for existing in self.routes.list_by_upstream(upstream_id).await? {
            if Some(existing.id) == exclude {
                continue;
            }
            let Some(other) = &existing.match_config.http else {
                continue;
            };
            if other.path != http.path || existing.priority != priority {
                continue;
            }
            if let Some(method) = other
                .methods
                .iter()
                .find(|method| http.methods.contains(method))
            {
                return Err(OagwError::new(
                    ErrorKind::RouteConflict,
                    format!(
                        "a route on this upstream already matches {method} {} at priority \
                         {priority}",
                        http.path
                    ),
                )
                .with(
                    "conflicting_route",
                    gts_helpers::anonymous_id(ROUTE_TYPE, existing.id),
                ));
            }
        }
        Ok(())
    }

    /// When an alias already exists on a visible ancestor, a create/replace is
    /// a *bind* to that ancestor's upstream, and the ancestor's `enforce`
    /// blocks are not overridable (`DESIGN.md` § *CRUD Semantics*).
    async fn check_ancestor_bind(
        &self,
        ctx: &SecurityContext,
        tenant_id: Uuid,
        alias: &str,
        spec: &UpstreamSpec,
    ) -> OagwResult<()> {
        for ancestor in self.tenants.ancestors(ctx, tenant_id).await {
            let Some(existing) = self.upstreams.find_by_alias(ancestor, alias).await? else {
                continue;
            };
            // `private` hides the ancestor's configuration entirely, so
            // there is nothing to bind to and nothing to enforce.
            if let Some(auth) = &existing.auth
                && auth.sharing == SharingMode::Enforce
                && spec.auth.is_some()
            {
                return Err(OagwError::field(
                    "auth",
                    format!(
                        "ancestor tenant {ancestor} shares alias {alias:?} with \
                         `auth.sharing: enforce`; the auth configuration cannot be overridden"
                    ),
                ));
            }
            if let Some(cors) = &existing.cors
                && cors.sharing == SharingMode::Enforce
                && spec.cors.is_some()
            {
                return Err(OagwError::field(
                    "cors",
                    format!(
                        "ancestor tenant {ancestor} shares alias {alias:?} with \
                         `cors.sharing: enforce`; the CORS configuration cannot be overridden"
                    ),
                ));
            }
        }
        Ok(())
    }
}

/// `404` for a management-API upstream miss.
fn upstream_not_found(id: Uuid) -> OagwError {
    OagwError::not_found(format!("upstream {id} not found"))
        .with("upstream_id", gts_helpers::anonymous_id(UPSTREAM_TYPE, id))
}

/// `404` for a management-API route miss.
fn route_not_found(id: Uuid) -> OagwError {
    OagwError::not_found(format!("route {id} not found"))
        .with("route_id", gts_helpers::anonymous_id(ROUTE_TYPE, id))
}

/// `409 PluginInUse`, shaped as the ADR documents it.
fn plugin_in_use(plugin_ref: &str, usage: &PluginUsage) -> OagwError {
    OagwError::new(
        ErrorKind::PluginInUse,
        format!(
            "Plugin is referenced by {} upstream(s) and {} route(s)",
            usage.upstreams.len(),
            usage.routes.len()
        ),
    )
    .with("plugin_id", plugin_ref.to_owned())
    .with(
        "referenced_by",
        serde_json::json!({
            "upstreams": usage.upstreams,
            "routes": usage.routes,
        }),
    )
}

/// Accept `auth`/`guard`/`transform` or the corresponding GTS base type.
fn parse_plugin_kind(raw: &str) -> OagwResult<PluginKind> {
    if let Some(kind) = PluginKind::from_str_opt(raw) {
        return Ok(kind);
    }
    if let Some(kind) = PluginKind::from_plugin_ref(raw) {
        return Ok(kind);
    }
    Err(OagwError::field(
        "plugin_type",
        format!("plugin_type must be one of [auth, guard, transform]: {raw:?}"),
    ))
}

/// Normalise and validate discovery tags against `^[a-z0-9_-]+$`.
fn normalize_tags(tags: Vec<String>) -> OagwResult<Vec<String>> {
    let mut normalized = Vec::with_capacity(tags.len());
    for tag in tags {
        let tag = tag.trim().to_ascii_lowercase();
        if tag.is_empty()
            || !tag
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
        {
            return Err(OagwError::field(
                "tags",
                format!("tag must match ^[a-z0-9_-]+$: {tag:?}"),
            ));
        }
        if !normalized.contains(&tag) {
            normalized.push(tag);
        }
    }
    Ok(normalized)
}
