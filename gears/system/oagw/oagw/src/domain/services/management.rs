//! Control-plane service (DESIGN §3.3 "CRUD Semantics" + §3.2 "Hierarchical
//! Configuration").
//!
//! All management operations are strictly scoped to the calling tenant
//! (ancestor resources are invisible, 404) except the documented "bind" path:
//! creating an upstream whose alias matches an ancestor's is a binding
//! operation that requires the `bind` permission and respects sharing-mode
//! constraints (`enforce` blocks overrides, `private` blocks visibility).

use std::sync::Arc;

use arc_swap::ArcSwap;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::config::OagwConfig;
use crate::domain::alias;
use crate::domain::dto::ListParams;
use crate::domain::error::{OagwError, OagwResult};
use crate::domain::list::{self, ListItem};
use crate::domain::merge::ResolvedChain;
use crate::domain::models::{
    AuthConfig, CorsConfig, Endpoint, Plugin, RateLimitConfig, Route, Scheme, SharingMode, Stored,
    Upstream, protocol_gts,
};
use crate::domain::scopes;
use crate::infra::storage::{ControlPlaneStore, SharedStore};

/// Validates that a plugin reference (named built-in or stored custom
/// UUID-backed plugin) resolves and matches its binding family.
pub trait PluginValidator: Send + Sync {
    /// Verify `plugin_ref` resolves for `tenant_id`.
    ///
    /// # Errors
    ///
    /// Returns a validation [`OagwError`] for unknown, catalog-only or
    /// family-mismatched references.
    fn validate_ref(&self, tenant_id: Uuid, plugin_ref: &str) -> OagwResult<()>;
}

/// Control-plane service.
pub struct ControlPlaneService {
    store: SharedStore,
    tenant_resolver: Option<Arc<dyn tenant_resolver_sdk::TenantResolverClient>>,
    config: ArcSwap<OagwConfig>,
    plugins: Arc<dyn PluginValidator>,
}

impl std::fmt::Debug for ControlPlaneService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ControlPlaneService")
            .field("store", &"ControlPlaneStore")
            .field("tenant_resolver", &self.tenant_resolver.is_some())
            .field("config", &self.config.load())
            .field("plugins", &"PluginValidator")
            .finish()
    }
}

impl ControlPlaneService {
    /// Create the control-plane service.
    #[must_use]
    pub fn new(
        store: SharedStore,
        tenant_resolver: Option<Arc<dyn tenant_resolver_sdk::TenantResolverClient>>,
        config: Arc<OagwConfig>,
        plugins: Arc<dyn PluginValidator>,
    ) -> Self {
        Self {
            store,
            tenant_resolver,
            config: ArcSwap::from(config),
            plugins,
        }
    }

    /// The in-memory store (shared with the data plane).
    #[must_use]
    pub fn store(&self) -> &ControlPlaneStore {
        &self.store
    }

    /// Tenant hierarchy for `tenant_id`, ordered descendant → root.
    /// Best-effort: a missing or failing resolver yields `[tenant_id]`.
    pub async fn tenant_chain(&self, ctx: &SecurityContext, tenant_id: Uuid) -> Vec<Uuid> {
        let mut chain = vec![tenant_id];
        if let Some(resolver) = &self.tenant_resolver {
            use tenant_resolver_sdk::{GetAncestorsOptions, TenantId};
            if let Ok(resp) = resolver
                .get_ancestors(ctx, TenantId(tenant_id), &GetAncestorsOptions::default())
                .await
            {
                chain.extend(resp.ancestors.into_iter().map(|t| t.id.0));
            }
        }
        chain
    }

    /// Resolve an alias to a merged upstream chain (DESIGN "Alias Resolution").
    ///
    /// Walks the tenant hierarchy descendant → root, collecting every enabled
    /// same-alias upstream; the closest one becomes the routing target
    /// (shadowing). Levels are returned root → descendant. Route matching is
    /// performed separately by [`resolve_route`](Self::resolve_route) with
    /// the concrete method/path of the request.
    ///
    /// # Errors
    ///
    /// Returns `RouteNotFound` when no enabled upstream owns the alias.
    pub async fn resolve_chain(
        &self,
        ctx: &SecurityContext,
        alias: &str,
    ) -> OagwResult<ResolvedChain> {
        let normalized = alias::normalize_alias(alias);
        if normalized.is_empty() {
            return Err(OagwError::RouteError {
                detail: "alias must not be empty".to_owned(),
            });
        }
        let chain = self.tenant_chain(ctx, ctx.subject_tenant_id()).await;
        let mut desc_to_root: Vec<Stored<Upstream>> = Vec::new();
        let mut selected: Option<Stored<Upstream>> = None;
        for tenant in chain {
            let Some(up) = self.store.upstream_by_alias(tenant, &normalized) else {
                continue;
            };
            if !up.record.enabled {
                continue;
            }
            if selected.is_none() {
                selected = Some(up.clone());
            }
            desc_to_root.push(up);
        }
        let selected = selected.ok_or_else(|| OagwError::RouteNotFound {
            alias: alias.to_owned(),
            path: String::new(),
        })?;

        let mut levels = Vec::with_capacity(desc_to_root.len());
        for up in desc_to_root.into_iter().rev() {
            let is_selected = up.record.id == selected.record.id;
            levels.push(crate::domain::merge::ChainLevel {
                is_selected,
                upstream: up,
                route: None,
            });
        }
        Ok(ResolvedChain {
            selected,
            levels,
            matched_route: None,
        })
    }

    /// Match a route within a resolved chain (DESIGN "Guard Rules" + "Request
    /// Routing"): descendant routes take priority, then longest path prefix,
    /// over the chain's enabled HTTP routes.
    #[must_use]
    pub fn resolve_route(
        &self,
        chain: &ResolvedChain,
        method: &http::Method,
        path: &str,
    ) -> Option<Stored<Route>> {
        let effective_path = if path.is_empty() { "/" } else { path };
        let mut best: Option<(usize, Stored<Route>)> = None;
        // Descendant-first: levels are root→desc, so iterate in reverse.
        for level in chain.levels.iter().rev() {
            for stored in self.store.routes_for_upstream(level.upstream.record.id?) {
                if !stored.record.enabled {
                    continue;
                }
                let route = &stored.record;
                let Some(http_match) = &route.match_.http else {
                    continue; // gRPC matching is out of scope (planned).
                };
                if !http_match
                    .methods
                    .iter()
                    .any(|m| m.eq_ignore_ascii_case(method.as_str()))
                {
                    continue;
                }
                if !route_matches_path(http_match.path.as_str(), effective_path) {
                    continue;
                }
                let len = http_match.path.len();
                if best.as_ref().map(|(blen, _)| *blen < len).unwrap_or(true) {
                    best = Some((len, stored));
                }
            }
        }
        best.map(|(_, r)| r)
    }

    // ------------------------------------------------------------------
    // Upstreams
    // ------------------------------------------------------------------

    /// Create an upstream (POST — DESIGN §3.3 CRUD Semantics).
    ///
    /// # Errors
    ///
    /// `400` on validation/alias enforcement failures, `403` on missing
    /// permission, `409` on alias conflict.
    pub async fn create_upstream(
        &self,
        ctx: &SecurityContext,
        mut body: Upstream,
    ) -> OagwResult<Stored<Upstream>> {
        scopes::require_scope(ctx, scopes::upstream::CREATE)?;
        let tenant_id = ctx.subject_tenant_id();
        body.id = None;

        self.validate_upstream(ctx, &body)?;

        let alias = alias::enforce_alias(&body.server.endpoints, body.alias.as_deref())
            .map_err(OagwError::validation)?;

        if self.store.upstream_by_alias(tenant_id, &alias).is_some() {
            return Err(OagwError::Conflict {
                detail: format!("upstream alias {alias:?} already exists in this tenant"),
            });
        }

        // Ancestor alias match => bind operation with sharing-mode constraints.
        if let Some(ancestor) = self.find_ancestor_upstream(ctx, tenant_id, &alias).await {
            self.enforce_bind(ctx, &ancestor, &body)?;
        }

        let id = Uuid::new_v4();
        body.id = Some(id);
        body.alias = Some(alias);
        let stored = Stored::new(tenant_id, body);
        self.store
            .insert_upstream(stored.clone())
            .map_err(|()| OagwError::Conflict {
                detail: "upstream alias already exists in this tenant".to_owned(),
            })?;
        Ok(stored)
    }

    /// Get an upstream by id (tenant-scoped).
    ///
    /// # Errors
    ///
    /// `404` when the upstream is missing or belongs to another tenant.
    pub async fn get_upstream(
        &self,
        ctx: &SecurityContext,
        id: Uuid,
    ) -> OagwResult<Stored<Upstream>> {
        scopes::require_scope(ctx, scopes::upstream::READ)?;
        self.store
            .upstream_by_id(ctx.subject_tenant_id(), id)
            .ok_or_else(|| OagwError::NotFound {
                resource: "upstream".to_owned(),
            })
    }

    /// List the calling tenant's upstreams with OData-style query parameters.
    ///
    /// # Errors
    ///
    /// `400` on unsupported filter/orderby/select or invalid `$top`.
    pub async fn list_upstreams(
        &self,
        ctx: &SecurityContext,
        params: &ListParams,
    ) -> OagwResult<(Vec<Stored<Upstream>>, usize)> {
        scopes::require_scope(ctx, scopes::upstream::READ)?;
        let items = self.store.list_upstreams(ctx.subject_tenant_id());
        list::apply_params(params, Stored::<Upstream>::list_fields(), items)
    }

    /// Replace an upstream (PUT — DESIGN "Alias Update Behavior").
    ///
    /// # Errors
    ///
    /// `404` when missing or foreign; `400` when the alias would change or
    /// sharing-mode constraints are violated.
    pub async fn replace_upstream(
        &self,
        ctx: &SecurityContext,
        id: Uuid,
        mut body: Upstream,
    ) -> OagwResult<Stored<Upstream>> {
        scopes::require_scope(ctx, scopes::upstream::OVERRIDE)?;
        let tenant_id = ctx.subject_tenant_id();
        let existing =
            self.store
                .upstream_by_id(tenant_id, id)
                .ok_or_else(|| OagwError::NotFound {
                    resource: "upstream".to_owned(),
                })?;

        body.id = Some(id);
        self.validate_upstream(ctx, &body)?;

        let effective_alias = alias::enforce_alias_update(
            existing.record.alias.as_deref().unwrap_or_default(),
            &existing.record.server.endpoints,
            &body,
        )
        .map_err(OagwError::validation)?;
        body.alias = Some(effective_alias.clone());

        if let Some(ancestor) = self
            .find_ancestor_upstream(ctx, tenant_id, &effective_alias)
            .await
        {
            self.enforce_bind(ctx, &ancestor, &body)?;
        }

        let stored = Stored::new(tenant_id, body);
        self.store.replace_upstream(stored.clone());
        Ok(stored)
    }

    /// Delete an upstream and cascade its routes (DELETE).
    ///
    /// # Errors
    ///
    /// `404` when missing or foreign.
    pub async fn delete_upstream(&self, ctx: &SecurityContext, id: Uuid) -> OagwResult<()> {
        scopes::require_scope(ctx, scopes::upstream::DELETE)?;
        if self
            .store
            .remove_upstream(ctx.subject_tenant_id(), id)
            .is_none()
        {
            return Err(OagwError::NotFound {
                resource: "upstream".to_owned(),
            });
        }
        Ok(())
    }

    // ------------------------------------------------------------------
    // Routes
    // ------------------------------------------------------------------

    /// Create a route (POST).
    ///
    /// # Errors
    ///
    /// `400` on validation failure, `403` on missing permission, `404` when
    /// the referenced upstream is not owned by the tenant, `409` on match
    /// conflict.
    pub async fn create_route(
        &self,
        ctx: &SecurityContext,
        mut body: Route,
    ) -> OagwResult<Stored<Route>> {
        scopes::require_scope(ctx, scopes::route::CREATE)?;
        let tenant_id = ctx.subject_tenant_id();
        body.id = None;

        self.validate_route(ctx, &body)?;
        // `upstream_id` must belong to the calling tenant.
        if self
            .store
            .upstream_by_id(tenant_id, body.upstream_id)
            .is_none()
        {
            return Err(OagwError::NotFound {
                resource: "upstream".to_owned(),
            });
        }
        self.check_route_match_unique(tenant_id, body.upstream_id, None, &body)?;

        let id = Uuid::new_v4();
        body.id = Some(id);
        let stored = Stored::new(tenant_id, body);
        self.store.insert_route(stored.clone());
        Ok(stored)
    }

    /// Get a route by id (tenant-scoped).
    ///
    /// # Errors
    ///
    /// `404` when missing or foreign.
    pub async fn get_route(&self, ctx: &SecurityContext, id: Uuid) -> OagwResult<Stored<Route>> {
        scopes::require_scope(ctx, scopes::route::READ)?;
        self.store
            .route_by_id(ctx.subject_tenant_id(), id)
            .ok_or_else(|| OagwError::NotFound {
                resource: "route".to_owned(),
            })
    }

    /// List the calling tenant's routes with OData-style query parameters.
    ///
    /// # Errors
    ///
    /// `400` on unsupported query expressions or invalid `$top`.
    pub async fn list_routes(
        &self,
        ctx: &SecurityContext,
        params: &ListParams,
    ) -> OagwResult<(Vec<Stored<Route>>, usize)> {
        scopes::require_scope(ctx, scopes::route::READ)?;
        let items = self.store.list_routes(ctx.subject_tenant_id());
        list::apply_params(params, Stored::<Route>::list_fields(), items)
    }

    /// Replace a route (PUT). `upstream_id` is immutable.
    ///
    /// # Errors
    ///
    /// `404` when missing or foreign; `400` when validation fails or
    /// `upstream_id` is changed; `409` on match conflict.
    pub async fn replace_route(
        &self,
        ctx: &SecurityContext,
        id: Uuid,
        mut body: Route,
    ) -> OagwResult<Stored<Route>> {
        scopes::require_scope(ctx, scopes::route::OVERRIDE)?;
        let tenant_id = ctx.subject_tenant_id();
        let existing =
            self.store
                .route_by_id(tenant_id, id)
                .ok_or_else(|| OagwError::NotFound {
                    resource: "route".to_owned(),
                })?;

        // `upstream_id` is immutable: absent in the update DTO, or must match.
        if body.upstream_id != Uuid::nil() && body.upstream_id != existing.record.upstream_id {
            return Err(OagwError::validation(
                "route upstream_id is immutable after creation",
            ));
        }
        body.upstream_id = existing.record.upstream_id;
        body.id = Some(id);

        self.validate_route(ctx, &body)?;
        self.check_route_match_unique(tenant_id, body.upstream_id, Some(id), &body)?;

        let stored = Stored::new(tenant_id, body);
        self.store.replace_route(stored.clone());
        Ok(stored)
    }

    /// Delete a route (DELETE).
    ///
    /// # Errors
    ///
    /// `404` when missing or foreign.
    pub async fn delete_route(&self, ctx: &SecurityContext, id: Uuid) -> OagwResult<()> {
        scopes::require_scope(ctx, scopes::route::DELETE)?;
        if self
            .store
            .remove_route(ctx.subject_tenant_id(), id)
            .is_none()
        {
            return Err(OagwError::NotFound {
                resource: "route".to_owned(),
            });
        }
        Ok(())
    }

    // ------------------------------------------------------------------
    // Plugins (custom, UUID-backed)
    // ------------------------------------------------------------------

    /// Create a custom plugin (POST — DESIGN "Plugin Lifecycle Management").
    ///
    /// # Errors
    ///
    /// `403` on missing family permission, `400` on validation, `409` on
    /// tenant-unique name conflict.
    pub async fn create_plugin(
        &self,
        ctx: &SecurityContext,
        mut body: Plugin,
    ) -> OagwResult<Stored<Plugin>> {
        let tenant_id = ctx.subject_tenant_id();
        require_plugin_action(ctx, &body.plugin_type, PluginAction::Create)?;
        self.validate_plugin(&body)?;

        let id = Uuid::new_v4();
        body.id = Some(id);
        let stored = Stored::new(tenant_id, body);
        self.store
            .insert_plugin(stored.clone())
            .map_err(|()| OagwError::Conflict {
                detail: "plugin name already exists in this tenant".to_owned(),
            })?;
        Ok(stored)
    }

    /// Get a custom plugin by id (tenant-scoped).
    ///
    /// # Errors
    ///
    /// `404` when missing or foreign.
    pub async fn get_plugin(&self, ctx: &SecurityContext, id: Uuid) -> OagwResult<Stored<Plugin>> {
        let stored = self
            .store
            .plugin_by_id(ctx.subject_tenant_id(), id)
            .ok_or_else(|| OagwError::NotFound {
                resource: "plugin".to_owned(),
            })?;
        require_plugin_action(ctx, &stored.record.plugin_type, PluginAction::Read)?;
        Ok(stored)
    }

    /// List custom plugins visible to the token (family-scoped read gates).
    ///
    /// # Errors
    ///
    /// `400` on unsupported query expressions or invalid `$top`.
    pub async fn list_plugins(
        &self,
        ctx: &SecurityContext,
        params: &ListParams,
    ) -> OagwResult<(Vec<Stored<Plugin>>, usize)> {
        let items = self.store.list_plugins(ctx.subject_tenant_id());
        let visible: Vec<Stored<Plugin>> = items
            .into_iter()
            .filter(|p| scopes::has_scope_for_family_read(ctx, &p.record.plugin_type))
            .collect();
        list::apply_params(params, Stored::<Plugin>::list_fields(), visible)
    }

    /// Delete a custom plugin; only unlinked plugins can be deleted.
    ///
    /// # Errors
    ///
    /// `404` when missing or foreign, `409` when the plugin is referenced by
    /// an upstream or route binding.
    pub async fn delete_plugin(&self, ctx: &SecurityContext, id: Uuid) -> OagwResult<()> {
        let stored = self
            .store
            .plugin_by_id(ctx.subject_tenant_id(), id)
            .ok_or_else(|| OagwError::NotFound {
                resource: "plugin".to_owned(),
            })?;
        require_plugin_action(ctx, &stored.record.plugin_type, PluginAction::Delete)?;

        if let Some(gts) = stored.record.gts_id() {
            if self.plugin_in_use(ctx.subject_tenant_id(), &gts) {
                return Err(OagwError::PluginInUse {
                    plugin_id: id.to_string(),
                });
            }
        }
        self.store
            .remove_plugin(ctx.subject_tenant_id(), id)
            .ok_or_else(|| OagwError::NotFound {
                resource: "plugin".to_owned(),
            })?;
        Ok(())
    }

    /// Get the Starlark source of a custom plugin.
    ///
    /// # Errors
    ///
    /// `404` when missing or foreign.
    pub async fn get_plugin_source(&self, ctx: &SecurityContext, id: Uuid) -> OagwResult<String> {
        let stored = self.get_plugin(ctx, id).await?;
        Ok(stored.record.source_code.clone())
    }

    // ------------------------------------------------------------------
    // Validation
    // ------------------------------------------------------------------

    /// Structural + semantic validation for an upstream body.
    fn validate_upstream(&self, ctx: &SecurityContext, up: &Upstream) -> OagwResult<()> {
        if up.server.endpoints.is_empty() {
            return Err(OagwError::validation("server.endpoints must not be empty"));
        }
        let first = &up.server.endpoints[0];
        for ep in &up.server.endpoints {
            self.validate_endpoint(ep)?;
            if ep.scheme != first.scheme || ep.port != first.port {
                return Err(OagwError::validation(
                    "all endpoints must share the same scheme and port",
                ));
            }
        }
        if up.protocol != protocol_gts::HTTP && up.protocol != protocol_gts::GRPC {
            return Err(OagwError::validation(format!(
                "unsupported protocol {:?}",
                up.protocol
            )));
        }
        if let Some(auth) = &up.auth {
            self.validate_auth(ctx, auth)?;
        }
        if let Some(plugins) = &up.plugins {
            self.validate_plugin_bindings(ctx, &plugins.items)?;
        }
        if let Some(rl) = &up.rate_limit {
            self.validate_rate_limit(rl)?;
        }
        if let Some(cors) = &up.cors {
            self.validate_cors(cors)?;
        }
        Ok(())
    }

    /// Validate a single endpoint.
    fn validate_endpoint(&self, ep: &Endpoint) -> OagwResult<()> {
        if ep.host.is_empty() {
            return Err(OagwError::validation("endpoint host must not be empty"));
        }
        if !alias::validate_hostname(&ep.host) && !alias::is_ip(&ep.host) {
            return Err(OagwError::validation(format!(
                "endpoint host {:?} is not a valid hostname or IP address",
                ep.host
            )));
        }
        if ep.port == 0 {
            return Err(OagwError::validation("endpoint port must be 1-65535"));
        }
        if ep.scheme == Scheme::Http && !self.config.load().allow_http_upstream {
            return Err(OagwError::validation(
                "http endpoint scheme is disabled; set allow_http_upstream to enable it",
            ));
        }
        Ok(())
    }

    /// Validate an auth config and its plugin reference.
    fn validate_auth(&self, ctx: &SecurityContext, auth: &AuthConfig) -> OagwResult<()> {
        if !auth.config.is_object() {
            return Err(OagwError::validation("auth.config must be a JSON object"));
        }
        let plugin_type = auth.resolved_type();
        if plugin_type != crate::domain::models::plugin_gts::AUTH_NOOP {
            self.plugins
                .validate_ref(ctx.subject_tenant_id(), plugin_type)?;
        }
        Ok(())
    }

    /// Validate every plugin binding reference.
    fn validate_plugin_bindings(&self, ctx: &SecurityContext, items: &[String]) -> OagwResult<()> {
        let tenant_id = ctx.subject_tenant_id();
        for item in items {
            self.plugins.validate_ref(tenant_id, item)?;
        }
        Ok(())
    }

    /// Validate a rate-limit config (schema requires `sustained`).
    fn validate_rate_limit(&self, rl: &RateLimitConfig) -> OagwResult<()> {
        let Some(sustained) = &rl.sustained else {
            return Err(OagwError::validation("rate_limit.sustained is required"));
        };
        if sustained.rate == 0 {
            return Err(OagwError::validation(
                "rate_limit.sustained.rate must be >= 1",
            ));
        }
        if rl.cost == 0 {
            return Err(OagwError::validation("rate_limit.cost must be >= 1"));
        }
        // `sliding_window` is accepted here and treated as a token bucket at
        // runtime (documented limitation; both are transient window buckets).
        Ok(())
    }

    /// Validate a CORS config (ADR-0004).
    fn validate_cors(&self, cors: &CorsConfig) -> OagwResult<()> {
        if cors.allow_credentials && cors.allowed_origins.iter().any(|o| o == "*") {
            return Err(OagwError::validation(
                "CORS allow_credentials cannot be combined with wildcard origins",
            ));
        }
        Ok(())
    }

    /// Validate a route body.
    fn validate_route(&self, ctx: &SecurityContext, route: &Route) -> OagwResult<()> {
        let has_http = route.match_.http.is_some();
        let has_grpc = route.match_.grpc.is_some();
        if has_http == has_grpc {
            return Err(OagwError::validation(
                "route match must specify exactly one of http or grpc",
            ));
        }
        if let Some(http_match) = &route.match_.http {
            if http_match.methods.is_empty() {
                return Err(OagwError::validation(
                    "route match.http.methods must not be empty",
                ));
            }
            for m in &http_match.methods {
                if !is_http_method(m) {
                    return Err(OagwError::validation(format!(
                        "route match.http.methods contains invalid method {m:?}"
                    )));
                }
            }
            if !http_match.path.starts_with('/') {
                return Err(OagwError::validation(
                    "route match.http.path must start with '/'",
                ));
            }
        }
        if let Some(grpc_match) = &route.match_.grpc {
            if grpc_match.service.is_empty() || grpc_match.method.is_empty() {
                return Err(OagwError::validation(
                    "route match.grpc requires non-empty service and method",
                ));
            }
        }
        if let Some(plugins) = &route.plugins {
            self.validate_plugin_bindings(ctx, &plugins.items)?;
        }
        if let Some(rl) = &route.rate_limit {
            self.validate_rate_limit(rl)?;
        }
        if let Some(cors) = &route.cors {
            self.validate_cors(cors)?;
        }
        Ok(())
    }

    /// Validate a custom plugin body.
    fn validate_plugin(&self, plugin: &Plugin) -> OagwResult<()> {
        if !plugin.valid_type() {
            return Err(OagwError::validation(format!(
                "unsupported plugin type {:?}; expected auth, guard or transform",
                plugin.plugin_type
            )));
        }
        if plugin.name.is_empty() {
            return Err(OagwError::validation("plugin name must not be empty"));
        }
        if !plugin.config_schema.is_object() {
            return Err(OagwError::validation(
                "plugin config_schema must be a JSON Schema object",
            ));
        }
        if plugin.source_code.trim().is_empty() {
            return Err(OagwError::validation(
                "plugin source_code (Starlark) must not be empty",
            ));
        }
        Ok(())
    }

    /// Enforce route match uniqueness within an upstream (DESIGN "Key
    /// Invariants": no two enabled routes may share `(path, method)`).
    fn check_route_match_unique(
        &self,
        tenant_id: Uuid,
        upstream_id: Uuid,
        exclude: Option<Uuid>,
        new: &Route,
    ) -> OagwResult<()> {
        let conflicts: Vec<Option<Uuid>> = self
            .store
            .list_routes(tenant_id)
            .into_iter()
            .filter(|r| r.record.upstream_id == upstream_id)
            .filter(|r| r.record.id != exclude)
            .filter(|r| routes_conflict(&r.record, new))
            .map(|r| r.record.id)
            .collect();
        if let Some(conflict) = conflicts.first() {
            return Err(OagwError::Conflict {
                detail: format!(
                    "route match conflicts with existing route {conflict:?} under the same upstream"
                ),
            });
        }
        Ok(())
    }

    /// Whether a custom plugin is referenced by any tenant resource.
    fn plugin_in_use(&self, tenant_id: Uuid, gts: &str) -> bool {
        let upstream_use = self.store.list_upstreams(tenant_id).iter().any(|u| {
            u.record
                .plugins
                .as_ref()
                .is_some_and(|p| p.items.iter().any(|i| i == gts))
        });
        let route_use = self.store.list_routes(tenant_id).iter().any(|r| {
            r.record
                .plugins
                .as_ref()
                .is_some_and(|p| p.items.iter().any(|i| i == gts))
        });
        upstream_use || route_use
    }

    /// Find the closest ancestor upstream owning the same alias (the "bind"
    /// target). Ancestors are ordered direct-parent → root.
    async fn find_ancestor_upstream(
        &self,
        ctx: &SecurityContext,
        tenant_id: Uuid,
        alias: &str,
    ) -> Option<Stored<Upstream>> {
        for ancestor in self.tenant_chain(ctx, tenant_id).await.into_iter().skip(1) {
            if let Some(up) = self.store.upstream_by_alias(ancestor, alias) {
                return Some(up);
            }
        }
        None
    }

    /// Enforce bind semantics against an ancestor upstream (DESIGN §3.2
    /// "Permissions and Access Control"): `enforce` blocks overrides,
    /// granular override permissions are checked.
    fn enforce_bind(
        &self,
        ctx: &SecurityContext,
        ancestor: &Stored<Upstream>,
        body: &Upstream,
    ) -> OagwResult<()> {
        scopes::require_scope(ctx, scopes::upstream::BIND)?;

        // `enforce` auth/cors cannot be overridden by a binding descendant.
        if body.auth.is_some()
            && ancestor
                .record
                .auth
                .as_ref()
                .is_some_and(|a| a.sharing == SharingMode::Enforce)
        {
            return Err(OagwError::validation(
                "ancestor upstream enforces its auth config; overrides are not permitted",
            ));
        }
        if body.cors.is_some()
            && ancestor
                .record
                .cors
                .as_ref()
                .is_some_and(|c| c.sharing == SharingMode::Enforce)
        {
            return Err(OagwError::validation(
                "ancestor upstream enforces its CORS config; overrides are not permitted",
            ));
        }

        // Granular override permissions.
        if body.auth.is_some() && ancestor.record.auth.as_ref().is_some() {
            scopes::require_scope(ctx, scopes::upstream::OVERRIDE_AUTH)?;
        }
        if body.rate_limit.is_some() && ancestor.record.rate_limit.as_ref().is_some() {
            scopes::require_scope(ctx, scopes::upstream::OVERRIDE_RATE)?;
        }
        if body.plugins.as_ref().is_some_and(|p| !p.items.is_empty())
            && ancestor.record.plugins.as_ref().is_some()
        {
            scopes::require_scope(ctx, scopes::upstream::ADD_PLUGINS)?;
        }
        Ok(())
    }
}

/// Whether a route match conflicts with another (same upstream).
fn routes_conflict(a: &Route, b: &Route) -> bool {
    match (&a.match_.http, &b.match_.http) {
        (Some(ha), Some(hb)) => {
            ha.path == hb.path
                && ha
                    .methods
                    .iter()
                    .any(|m| hb.methods.iter().any(|n| m.eq_ignore_ascii_case(n)))
        }
        _ => match (&a.match_.grpc, &b.match_.grpc) {
            (Some(ga), Some(gb)) => ga.service == gb.service && ga.method == gb.method,
            _ => false,
        },
    }
}

/// Case-insensitive path-prefix match with a normalized request path.
fn route_matches_path(route_path: &str, request_path: &str) -> bool {
    let rp = route_path.trim_end_matches('/');
    if rp.is_empty() {
        return true; // "/" prefix matches everything.
    }
    request_path.starts_with(rp)
        && (request_path.as_bytes().get(rp.len()) == Some(&b'/')
            || request_path.as_bytes().get(rp.len()).is_none())
}

/// Whether `m` is a standard HTTP method token.
fn is_http_method(m: &str) -> bool {
    #[allow(clippy::match_like_matches_macro)]
    match m.to_ascii_uppercase().as_str() {
        "GET" | "POST" | "PUT" | "DELETE" | "PATCH" | "HEAD" | "OPTIONS" | "CONNECT" | "TRACE" => {
            true
        }
        _ => false,
    }
}

/// Plugin lifecycle action used for scope selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PluginAction {
    Create,
    Read,
    Delete,
}

/// Require the family-scoped action permission for a custom plugin.
fn require_plugin_action(
    ctx: &SecurityContext,
    family: &str,
    action: PluginAction,
) -> OagwResult<()> {
    let scope = match (family, action) {
        ("auth", PluginAction::Create) => scopes::plugin::auth::CREATE,
        ("auth", PluginAction::Read) => scopes::plugin::auth::READ,
        ("auth", PluginAction::Delete) => scopes::plugin::auth::DELETE,
        ("guard", PluginAction::Create) => scopes::plugin::guard::CREATE,
        ("guard", PluginAction::Read) => scopes::plugin::guard::READ,
        ("guard", PluginAction::Delete) => scopes::plugin::guard::DELETE,
        ("transform", PluginAction::Create) => scopes::plugin::transform::CREATE,
        ("transform", PluginAction::Read) => scopes::plugin::transform::READ,
        ("transform", PluginAction::Delete) => scopes::plugin::transform::DELETE,
        (other, _) => {
            return Err(OagwError::validation(format!(
                "unsupported plugin family {other:?}"
            )));
        }
    };
    scopes::require_scope(ctx, scope)
}

/// List-item projection for upstreams.
impl ListItem for Stored<Upstream> {
    fn list_fields() -> &'static [&'static str] {
        &["id", "alias", "enabled", "protocol"]
    }

    fn field_value(&self, field: &str) -> Option<String> {
        match field {
            "id" => self.record.id.map(|i| i.to_string()),
            "alias" => self.record.alias.clone(),
            "enabled" => Some(self.record.enabled.to_string()),
            "protocol" => Some(self.record.protocol.clone()),
            _ => None,
        }
    }
}

/// List-item projection for routes.
impl ListItem for Stored<Route> {
    fn list_fields() -> &'static [&'static str] {
        &["id", "upstream_id", "enabled"]
    }

    fn field_value(&self, field: &str) -> Option<String> {
        match field {
            "id" => Some(self.record.id?.to_string()),
            "upstream_id" => Some(self.record.upstream_id.to_string()),
            "enabled" => Some(self.record.enabled.to_string()),
            _ => None,
        }
    }
}

/// List-item projection for plugins.
impl ListItem for Stored<Plugin> {
    fn list_fields() -> &'static [&'static str] {
        &["id", "name", "type"]
    }

    fn field_value(&self, field: &str) -> Option<String> {
        match field {
            "id" => Some(self.record.id?.to_string()),
            "name" => Some(self.record.name.clone()),
            "type" => Some(self.record.plugin_type.clone()),
            _ => None,
        }
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use crate::domain::test_util::MockTenantResolver;

    struct NoopPluginValidator;

    impl PluginValidator for NoopPluginValidator {
        fn validate_ref(&self, _tenant_id: Uuid, _plugin_ref: &str) -> OagwResult<()> {
            Ok(())
        }
    }

    /// A control-plane service over a fresh store with a mocked tenant
    /// hierarchy (`tenant -> ancestors`).
    fn service_with(
        hierarchy: Vec<(Uuid, Vec<Uuid>)>,
    ) -> (Arc<ControlPlaneService>, Arc<ControlPlaneStore>) {
        let store: SharedStore = Arc::new(ControlPlaneStore::new());
        let resolver: Option<Arc<dyn tenant_resolver_sdk::TenantResolverClient>> =
            Some(Arc::new(MockTenantResolver::new(hierarchy)));
        let cp = Arc::new(ControlPlaneService::new(
            Arc::clone(&store),
            resolver,
            Arc::new(OagwConfig::default()),
            Arc::new(NoopPluginValidator),
        ));
        (cp, Arc::clone(&store))
    }

    fn ctx(tenant: Uuid) -> SecurityContext {
        SecurityContext::builder()
            .subject_id(Uuid::new_v4())
            .subject_tenant_id(tenant)
            .token_scopes(vec!["*".to_owned()])
            .build()
            .expect("valid security context")
    }

    /// Insert an upstream owned by `tenant` with the given alias and return its id.
    fn add_upstream(store: &ControlPlaneStore, tenant: Uuid, alias: &str, enabled: bool) -> Uuid {
        let id = Uuid::new_v4();
        let mut up = Upstream::default();
        up.id = Some(id);
        up.alias = Some(alias.to_owned());
        up.enabled = enabled;
        up.protocol = protocol_gts::HTTP.to_owned();
        store
            .insert_upstream(Stored::new(tenant, up))
            .expect("insert upstream");
        id
    }

    /// Insert an enabled HTTP route for `up_id` matching exactly one method+path.
    fn add_route(
        store: &ControlPlaneStore,
        tenant: Uuid,
        up_id: Uuid,
        method: &str,
        path: &str,
    ) -> Uuid {
        let id = Uuid::new_v4();
        let route = Route {
            id: Some(id),
            enabled: true,
            tags: Vec::new(),
            upstream_id: up_id,
            match_: crate::domain::models::MatchConfig {
                http: Some(crate::domain::models::HttpMatch {
                    methods: vec![method.to_owned()],
                    path: path.to_owned(),
                    query_allowlist: Vec::new(),
                    path_suffix_mode: crate::domain::models::PathSuffixMode::Append,
                }),
                grpc: None,
            },
            plugins: None,
            rate_limit: None,
            cors: None,
        };
        store.insert_route(Stored::new(tenant, route));
        id
    }

    #[tokio::test]
    async fn resolve_chain_descendant_shadows_ancestor() {
        let t1 = Uuid::new_v4();
        let t2 = Uuid::new_v4();
        let (cp, store) = service_with(vec![(t1, Vec::new()), (t2, vec![t1])]);

        let ancestor_id = add_upstream(&store, t1, "svc", true);
        let descendant_id = add_upstream(&store, t2, "svc", true);

        // The descendant's view selects its own upstream ...
        let chain = cp
            .resolve_chain(&ctx(t2), "svc")
            .await
            .expect("resolves for t2");
        assert_eq!(
            chain.selected.record.id,
            Some(descendant_id),
            "descendant wins (shadowing)"
        );
        assert_eq!(
            chain.levels.len(),
            2,
            "both levels present root->descendant"
        );
        assert_eq!(chain.levels[0].upstream.record.id, Some(ancestor_id));
        assert!(!chain.levels[0].is_selected);
        assert!(chain.levels[1].is_selected, "descendant level is selected");

        // ... while the ancestor sees only its own.
        let chain = cp
            .resolve_chain(&ctx(t1), "svc")
            .await
            .expect("resolves for t1");
        assert_eq!(chain.selected.record.id, Some(ancestor_id));
        assert_eq!(chain.levels.len(), 1);
    }

    #[tokio::test]
    async fn resolve_chain_disabled_descendant_falls_back_to_ancestor() {
        let t1 = Uuid::new_v4();
        let t2 = Uuid::new_v4();
        let (cp, store) = service_with(vec![(t1, Vec::new()), (t2, vec![t1])]);

        let ancestor_id = add_upstream(&store, t1, "svc", true);
        // A disabled descendant must not become the routing target.
        add_upstream(&store, t2, "svc", false);

        let chain = cp
            .resolve_chain(&ctx(t2), "svc")
            .await
            .expect("ancestor remains routable");
        assert_eq!(chain.selected.record.id, Some(ancestor_id));
        assert_eq!(
            chain.levels.len(),
            1,
            "disabled level is absent from the chain"
        );
    }

    #[tokio::test]
    async fn resolve_chain_unknown_alias_is_route_not_found() {
        let t1 = Uuid::new_v4();
        let (cp, store) = service_with(vec![(t1, Vec::new())]);
        add_upstream(&store, t1, "svc", true);

        let err = cp.resolve_chain(&ctx(t1), "ghost").await.unwrap_err();
        assert!(
            matches!(err, OagwError::RouteNotFound { .. }),
            "got {err:?}"
        );
        assert_eq!(err.status(), http::StatusCode::NOT_FOUND);
        assert_eq!(err.gts_type(), crate::domain::error::gts::ROUTE_NOT_FOUND);
    }

    #[tokio::test]
    async fn resolve_route_prefers_longest_prefix_and_descendant_on_ties() {
        let t1 = Uuid::new_v4();
        let t2 = Uuid::new_v4();
        let (cp, store) = service_with(vec![(t1, Vec::new()), (t2, vec![t1])]);

        let ancestor_id = add_upstream(&store, t1, "svc", true);
        let descendant_id = add_upstream(&store, t2, "svc", true);
        // Same-length "/" routes on both levels: the descendant must win.
        add_route(&store, t1, ancestor_id, "GET", "/");
        let descendant_broad = add_route(&store, t2, descendant_id, "GET", "/");
        // A longer ancestor prefix beats the broader descendant wildcard.
        let ancestor_precise = add_route(&store, t1, ancestor_id, "GET", "/v1");

        let chain = cp.resolve_chain(&ctx(t2), "svc").await.expect("chain");
        let route = cp
            .resolve_route(&chain, &http::Method::GET, "/chat")
            .expect("route");
        assert_eq!(
            route.record.id,
            Some(descendant_broad),
            "tie broken toward the descendant"
        );

        // "/v1/chat": both "/" (len 1) and "/v1" (len 3) match; longest wins.
        let route = cp
            .resolve_route(&chain, &http::Method::GET, "/v1/chat")
            .expect("route");
        assert_eq!(
            route.record.id,
            Some(ancestor_precise),
            "longest prefix wins across levels"
        );
    }

    #[tokio::test]
    async fn resolve_route_method_matching_is_case_insensitive_and_restrictive() {
        let t1 = Uuid::new_v4();
        let (cp, store) = service_with(vec![(t1, Vec::new())]);
        let up_id = add_upstream(&store, t1, "svc", true);
        add_route(&store, t1, up_id, "GET", "/api");

        let chain = cp.resolve_chain(&ctx(t1), "svc").await.expect("chain");

        // Case-insensitive method match.
        let route = cp
            .resolve_route(&chain, &"geT".parse::<http::Method>().unwrap(), "/api/x")
            .expect("route");
        assert_eq!(
            route.record.match_.http.as_ref().unwrap().methods,
            vec!["GET"]
        );

        // Unlisted method: no route.
        assert!(
            cp.resolve_route(&chain, &http::Method::POST, "/api/x")
                .is_none()
        );

        // Path that does not start with the prefix: no route.
        assert!(
            cp.resolve_route(&chain, &http::Method::GET, "/other")
                .is_none()
        );
    }
}
