//! Control plane service — tenant-scoped resource management (DESIGN §5).
//!
//! Responsible for:
//! - upstream CRUD with alias derivation / enforcement (PRD §5.5)
//! - route CRUD with match-rulet uniqueness
//! - custom plugin CRUD with in-use protection
//! - tenant-chain alias resolution (shadowing) and effective-config merging
//!   for the data plane

use std::sync::Arc;

use tenant_resolver_sdk::{TenantId, TenantResolverClient, TenantResolverError};
use uuid::Uuid;

use crate::config::OagwConfig;
use crate::domain::dto::{
    AuthConfig, BurstSpec, CorsConfig, EffectiveRoute, EffectiveUpstream, Endpoint, EndpointScheme,
    MatchConfig, PathSuffixMode, Plugin, PluginsConfig, RateLimitConfig, RateSpec, Route,
    SharingMode, Upstream,
};
use crate::domain::error::{DomainError, ProblemContext};
use crate::domain::gts_helpers;
use crate::domain::repo::{PluginRepository, RepoConflict, RouteRepository, UpstreamRepository};
use crate::infra::plugin::registry::{
    AuthPluginRegistry, GuardPluginRegistry, TransformPluginRegistry,
};
use crate::infra::storage::MemoryRepos;

use super::alias::{
    AliasDecision, derive_alias, is_ipv4, is_ipv6_candidate, is_valid_alias, is_valid_hostname,
};

/// Bundled repositories handed to the control plane.
pub type ControlPlaneRepos = MemoryRepos;

/// Fully-resolved proxy target: an effective upstream plus the single
/// matching effective route for a request.
#[derive(Debug, Clone)]
pub struct ResolvedTarget {
    /// Effective (merged) upstream configuration.
    pub upstream: EffectiveUpstream,
    /// Effective (merged) route configuration.
    pub route: EffectiveRoute,
}

/// Control plane service.
pub struct ControlPlaneService {
    repos: ControlPlaneRepos,
    tenant_resolver: Arc<dyn TenantResolverClient>,
    auth_registry: Arc<AuthPluginRegistry>,
    guard_registry: Arc<GuardPluginRegistry>,
    transform_registry: Arc<TransformPluginRegistry>,
    config: OagwConfig,
}

impl ControlPlaneService {
    /// Create the service.
    #[must_use]
    pub fn new(
        repos: ControlPlaneRepos,
        tenant_resolver: Arc<dyn TenantResolverClient>,
        auth_registry: Arc<AuthPluginRegistry>,
        guard_registry: Arc<GuardPluginRegistry>,
        transform_registry: Arc<TransformPluginRegistry>,
        config: OagwConfig,
    ) -> Self {
        Self {
            repos,
            tenant_resolver,
            auth_registry,
            guard_registry,
            transform_registry,
            config,
        }
    }

    /// Shared handle to the repositories (used by the data plane service).
    #[must_use]
    pub fn repos(&self) -> &ControlPlaneRepos {
        &self.repos
    }

    // ------------------------------------------------------------------
    // Upstreams
    // ------------------------------------------------------------------

    /// Create an upstream: derive/validate the alias, enforce ancestor bind
    /// constraints, store it.
    ///
    /// # Errors
    ///
    /// Returns a validation error when the alias or endpoints are invalid, an
    /// `AliasConflict` when another upstream of this tenant already holds the
    /// alias, an ancestor-enforcement error when a binding would override an
    /// enforced ancestor config, or an internal error on a repository
    /// conflict.
    pub async fn create_upstream(
        &self,
        ctx: &toolkit_security::SecurityContext,
        tenant_id: Uuid,
        input: Upstream,
    ) -> Result<Upstream, DomainError> {
        self.validate_upstream(tenant_id, &input)?;

        let endpoints = input.server.endpoints.clone();
        let alias = match (derive_alias(&endpoints), input.alias.clone()) {
            (AliasDecision::Derived(d), None) => d,
            (AliasDecision::Derived(d), Some(a)) => {
                let a = normalize_alias(&a);
                if a == d {
                    d
                } else {
                    return Err(validation_err(format!(
                        "alias '{a}' does not match the derived value '{d}'"
                    )));
                }
            }
            (AliasDecision::ExplicitRequired, Some(a)) => {
                let a = normalize_alias(&a);
                if !is_valid_alias(&a) {
                    return Err(validation_err(format!("invalid alias format: '{a}'")));
                }
                a
            }
            (AliasDecision::ExplicitRequired, None) => {
                return Err(validation_err(
                    "explicit alias required for these endpoints (non-derivable)",
                ));
            }
        };

        if self.repos.upstreams.list(tenant_id).len() >= self.config.max_upstreams_per_tenant {
            return Err(validation_err(format!(
                "upstream limit reached ({})",
                self.config.max_upstreams_per_tenant
            )));
        }

        self.enforce_ancestor_bindings(ctx, tenant_id, &alias, &input)
            .await?;

        let id = Uuid::new_v4();
        let upstream = Upstream {
            id: Some(id),
            tenant_id: Some(tenant_id),
            alias: Some(alias.clone()),
            ..input
        };
        match self.repos.upstreams.insert(upstream.clone()) {
            Ok(()) => Ok(upstream),
            Err(RepoConflict::DuplicateAlias) => Err(DomainError::AliasConflict {
                detail: format!("an upstream with alias '{alias}' already exists for this tenant"),
                context: Some(ProblemContext {
                    alias: Some(alias),
                    ..ProblemContext::new()
                }),
            }),
            Err(RepoConflict::DuplicateName) => Err(DomainError::internal(
                "unexpected duplicate-name conflict on upstream insert",
            )),
            Err(RepoConflict::None) => Err(DomainError::internal("unexpected insert state")),
        }
    }

    /// Replace an upstream: re-validate the alias and match rules.
    ///
    /// # Errors
    ///
    /// Returns a `NotFound` error when the upstream does not exist, a
    /// validation error when the new alias/endpoints are invalid, or an
    /// ancestor-enforcement error when a binding would override an enforced
    /// ancestor config.
    pub async fn update_upstream(
        &self,
        ctx: &toolkit_security::SecurityContext,
        tenant_id: Uuid,
        id: Uuid,
        input: Upstream,
    ) -> Result<Upstream, DomainError> {
        let existing = self
            .repos
            .upstreams
            .get(tenant_id, id)
            .ok_or_else(|| DomainError::not_found(format!("upstream {id} not found")))?;
        let existing_alias = existing.alias.clone().unwrap_or_default();
        let existing_endpoints = existing.server.endpoints.clone();

        self.validate_upstream(tenant_id, &input)?;

        let alias = enforce_alias_update(
            &existing_endpoints,
            &existing_alias,
            &input.server.endpoints,
            input.alias.as_deref(),
        )?;

        self.enforce_ancestor_bindings(ctx, tenant_id, &alias, &input)
            .await?;

        let updated = Upstream {
            id: Some(id),
            tenant_id: Some(tenant_id),
            alias: Some(alias),
            ..input
        };
        self.repos.upstreams.replace(updated.clone());
        Ok(updated)
    }

    /// Get an upstream owned by a tenant.
    ///
    /// # Errors
    ///
    /// Returns a `NotFound` error when no upstream with `id` exists for the
    /// tenant.
    #[allow(clippy::result_large_err)] // DomainError is a broad domain error enum; boxing would ripple through every caller
    pub fn get_upstream(&self, tenant_id: Uuid, id: Uuid) -> Result<Upstream, DomainError> {
        self.repos
            .upstreams
            .get(tenant_id, id)
            .ok_or_else(|| DomainError::not_found(format!("upstream {id} not found")))
    }

    /// List upstreams owned by a tenant.
    #[must_use]
    pub fn list_upstreams(&self, tenant_id: Uuid) -> Vec<Upstream> {
        self.repos.upstreams.list(tenant_id)
    }

    /// Get an upstream by alias (case-insensitive).
    ///
    /// # Errors
    ///
    /// Returns a `NotFound` error when no upstream with `alias` exists for the
    /// tenant.
    #[allow(clippy::result_large_err)] // DomainError is a broad domain error enum; boxing would ripple through every caller
    pub fn get_upstream_by_alias(
        &self,
        tenant_id: Uuid,
        alias: &str,
    ) -> Result<Upstream, DomainError> {
        self.repos
            .upstreams
            .get_by_alias(tenant_id, &normalize_alias(alias))
            .ok_or_else(|| {
                DomainError::not_found(format!("upstream with alias '{alias}' not found"))
            })
    }

    /// Delete an upstream (cascading to its routes).
    ///
    /// # Errors
    ///
    /// Returns a `NotFound` error when no upstream with `id` exists for the
    /// tenant.
    #[allow(clippy::result_large_err)] // DomainError is a broad domain error enum; boxing would ripple through every caller
    pub fn delete_upstream(&self, tenant_id: Uuid, id: Uuid) -> Result<(), DomainError> {
        if !self.repos.upstreams.delete(tenant_id, id) {
            return Err(DomainError::not_found(format!("upstream {id} not found")));
        }
        self.repos.routes.delete_by_upstream(tenant_id, id);
        Ok(())
    }

    // ------------------------------------------------------------------
    // Routes
    // ------------------------------------------------------------------

    /// Create a route under an upstream owned by the tenant.
    ///
    /// # Errors
    ///
    /// Returns a validation error when the route is invalid or its upstream
    /// does not exist for the tenant, or a `RouteConflict` when a route with
    /// the same path and overlapping method already exists.
    #[allow(clippy::result_large_err)] // DomainError is a broad domain error enum; boxing would ripple through every caller
    pub fn create_route(&self, tenant_id: Uuid, mut input: Route) -> Result<Route, DomainError> {
        self.validate_route(tenant_id, &input)?;
        let upstream_id = input.upstream_id;
        let id = Uuid::new_v4();
        input.id = Some(id);
        input.tenant_id = Some(tenant_id);
        match self.repos.routes.insert(input.clone()) {
            Ok(()) => Ok(input),
            Err(RepoConflict::DuplicateAlias) => Err(route_conflict_err(
                "a route with the same path and method already exists on this upstream",
                &upstream_id,
            )),
            Err(RepoConflict::DuplicateName) => Err(DomainError::internal(
                "unexpected duplicate-name conflict on route insert",
            )),
            Err(RepoConflict::None) => Err(DomainError::internal("unexpected insert state")),
        }
    }

    /// Replace a route (`upstream_id` is immutable).
    ///
    /// # Errors
    ///
    /// Returns a `NotFound` error when the route does not exist, a validation
    /// error when the replacement is invalid, or a `RouteConflict` when the
    /// replacement collides with another route on the same upstream.
    #[allow(clippy::result_large_err)] // DomainError is a broad domain error enum; boxing would ripple through every caller
    pub fn update_route(
        &self,
        tenant_id: Uuid,
        id: Uuid,
        input: Route,
    ) -> Result<Route, DomainError> {
        let existing = self
            .repos
            .routes
            .get(tenant_id, id)
            .ok_or_else(|| DomainError::not_found(format!("route {id} not found")))?;
        self.validate_route(tenant_id, &input)?;
        let updated = Route {
            id: Some(id),
            tenant_id: Some(tenant_id),
            upstream_id: existing.upstream_id,
            ..input
        };
        self.repos.routes.replace(updated.clone());
        Ok(updated)
    }

    /// Get a route by tenant + id.
    ///
    /// # Errors
    ///
    /// Returns a `NotFound` error when no route with `id` exists for the
    /// tenant.
    #[allow(clippy::result_large_err)] // DomainError is a broad domain error enum; boxing would ripple through every caller
    pub fn get_route(&self, tenant_id: Uuid, id: Uuid) -> Result<Route, DomainError> {
        self.repos
            .routes
            .get(tenant_id, id)
            .ok_or_else(|| DomainError::not_found(format!("route {id} not found")))
    }

    /// List routes owned by a tenant (optionally filtered by upstream).
    #[must_use]
    pub fn list_routes(&self, tenant_id: Uuid, upstream_id: Option<Uuid>) -> Vec<Route> {
        self.repos.routes.list(tenant_id, upstream_id)
    }

    /// Delete a route.
    ///
    /// # Errors
    ///
    /// Returns a `NotFound` error when no route with `id` exists for the
    /// tenant.
    #[allow(clippy::result_large_err)] // DomainError is a broad domain error enum; boxing would ripple through every caller
    pub fn delete_route(&self, tenant_id: Uuid, id: Uuid) -> Result<(), DomainError> {
        if !self.repos.routes.delete(tenant_id, id) {
            return Err(DomainError::not_found(format!("route {id} not found")));
        }
        Ok(())
    }

    // ------------------------------------------------------------------
    // Plugins
    // ------------------------------------------------------------------

    /// Create a custom plugin (immutable after creation — no update).
    ///
    /// # Errors
    ///
    /// Returns a validation error when the plugin type or name is invalid or
    /// the plugin limit is reached, or an `AliasConflict` when another plugin
    /// of this tenant already carries the name.
    #[allow(clippy::result_large_err)] // DomainError is a broad domain error enum; boxing would ripple through every caller
    pub fn create_plugin(&self, tenant_id: Uuid, mut input: Plugin) -> Result<Plugin, DomainError> {
        if !matches!(input.plugin_type.as_str(), "auth" | "guard" | "transform") {
            return Err(validation_err(format!(
                "plugin_type must be one of 'auth', 'guard', 'transform' (got '{}')",
                input.plugin_type
            )));
        }
        if input.name.trim().is_empty() {
            return Err(validation_err("plugin name is required"));
        }
        if self.repos.plugins.list(tenant_id).len() >= self.config.max_plugins_per_tenant {
            return Err(validation_err("plugin limit reached"));
        }
        let id = Uuid::new_v4();
        input.id = Some(id);
        input.tenant_id = Some(tenant_id);
        match self.repos.plugins.insert(input.clone()) {
            Ok(()) => Ok(input),
            Err(RepoConflict::DuplicateName) => Err(DomainError::AliasConflict {
                detail: format!(
                    "a plugin named '{}' already exists for this tenant",
                    input.name
                ),
                context: None,
            }),
            Err(RepoConflict::DuplicateAlias) => Err(DomainError::internal(
                "unexpected duplicate-alias conflict on plugin insert",
            )),
            Err(RepoConflict::None) => Err(DomainError::internal("unexpected insert state")),
        }
    }

    /// Get a custom plugin.
    ///
    /// # Errors
    ///
    /// Returns a `NotFound` error when no plugin with `id` exists for the
    /// tenant.
    #[allow(clippy::result_large_err)] // DomainError is a broad domain error enum; boxing would ripple through every caller
    pub fn get_plugin(&self, tenant_id: Uuid, id: Uuid) -> Result<Plugin, DomainError> {
        self.repos
            .plugins
            .get(tenant_id, id)
            .ok_or_else(|| DomainError::not_found(format!("plugin {id} not found")))
    }

    /// List custom plugins owned by a tenant.
    #[must_use]
    pub fn list_plugins(&self, tenant_id: Uuid) -> Vec<Plugin> {
        self.repos.plugins.list(tenant_id)
    }

    /// Delete a custom plugin. Returns 409 `PluginInUse` when referenced.
    ///
    /// # Errors
    ///
    /// Returns a `NotFound` error when no plugin with `id` exists for the
    /// tenant, or a `PluginInUse` error when the plugin is still referenced by
    /// an upstream or route.
    #[allow(clippy::result_large_err)] // DomainError is a broad domain error enum; boxing would ripple through every caller
    pub fn delete_plugin(&self, tenant_id: Uuid, id: Uuid) -> Result<(), DomainError> {
        let plugin = self
            .repos
            .plugins
            .get(tenant_id, id)
            .ok_or_else(|| DomainError::not_found(format!("plugin {id} not found")))?;
        let gts = plugin.gts_id();

        let mut upstream_refs = Vec::new();
        let mut route_refs = Vec::new();

        for u in self.repos.upstreams.list(tenant_id) {
            let referenced = u
                .auth
                .as_ref()
                .is_some_and(|a| a.plugin_type.as_deref() == Some(gts.as_str()))
                || u.plugins
                    .as_ref()
                    .is_some_and(|p| p.items.iter().any(|b| b.plugin_ref == gts));
            if referenced {
                upstream_refs.push(
                    u.alias
                        .unwrap_or_else(|| u.id.unwrap_or_default().to_string()),
                );
            }
        }
        for r in self.repos.routes.list(tenant_id, None) {
            let referenced = r
                .plugins
                .as_ref()
                .is_some_and(|p| p.items.iter().any(|b| b.plugin_ref == gts));
            if referenced {
                route_refs.push(r.id.unwrap_or_default().to_string());
            }
        }

        if !upstream_refs.is_empty() || !route_refs.is_empty() {
            return Err(DomainError::PluginInUse {
                detail: format!(
                    "plugin '{}' is referenced by {} upstream(s) and {} route(s)",
                    plugin.name,
                    upstream_refs.len(),
                    route_refs.len()
                ),
                plugin_id: gts,
                referenced_by: crate::domain::error::ReferencedBy {
                    upstreams: upstream_refs,
                    routes: route_refs,
                },
            });
        }

        if !self.repos.plugins.delete(tenant_id, id) {
            return Err(DomainError::not_found(format!("plugin {id} not found")));
        }
        Ok(())
    }

    // ------------------------------------------------------------------
    // Resolution (data-plane facing)
    // ------------------------------------------------------------------

    /// Resolve the closest enabled upstream for `alias` walking the tenant
    /// chain descendant → root (shadowing, DESIGN §3.5). Returns the
    /// effective (merged) upstream configuration.
    ///
    /// # Errors
    ///
    /// Returns a `RouteNotFound` error when the tenant chain resolution fails
    /// or no enabled upstream with `alias` exists along the chain.
    pub async fn resolve_upstream(
        &self,
        ctx: &toolkit_security::SecurityContext,
        tenant_id: Uuid,
        alias: &str,
    ) -> Result<EffectiveUpstream, DomainError> {
        let chain = self.tenant_chain(ctx, tenant_id).await?;
        let bindings = self.upstream_bindings(&chain, alias);
        if bindings.is_empty() {
            return Err(route_not_found(format!("no upstream with alias '{alias}'")));
        }
        if !bindings[0].enabled {
            return Err(link_unavailable(format!(
                "upstream with alias '{alias}' is disabled"
            )));
        }
        Ok(self.build_effective_upstream(&bindings, alias))
    }

    /// Resolve the full proxy target: effective upstream plus the matching
    /// effective route for `method` + `path`.
    ///
    /// # Errors
    ///
    /// Returns a `RouteNotFound` error when the tenant chain resolution fails,
    /// no enabled upstream with `alias` exists along the chain, or no route
    /// matches `method` + `path`.
    pub async fn resolve_proxy_target(
        &self,
        ctx: &toolkit_security::SecurityContext,
        tenant_id: Uuid,
        alias: &str,
        method: &str,
        path: &str,
    ) -> Result<ResolvedTarget, DomainError> {
        let chain = self.tenant_chain(ctx, tenant_id).await?;
        let bindings = self.upstream_bindings(&chain, alias);
        if bindings.is_empty() {
            return Err(route_not_found(format!("no upstream with alias '{alias}'")));
        }
        if !bindings[0].enabled {
            return Err(link_unavailable(format!(
                "upstream with alias '{alias}' is disabled"
            )));
        }
        let upstream_eff = self.build_effective_upstream(&bindings, alias);

        // Find the best route. Descendant tenants iterate first (shadowing),
        // so a route from an ancestor tenant never replaces a descendant's
        // match; within one tenant the longest path-prefix wins.
        let mut best: Option<(Route, Uuid)> = None;
        for tid in &chain {
            let Some(binding) = bindings
                .iter()
                .rev()
                .find(|b| b.tenant_id.unwrap_or_default() == *tid)
            else {
                continue;
            };
            let up_id = binding.id.unwrap_or_default();
            for r in self.repos.routes.list(*tid, Some(up_id)) {
                if route_matches(&r, method, path) {
                    let replace = match &best {
                        None => true,
                        Some((b, bt)) if *bt == *tid => route_path_len(&r) > route_path_len(b),
                        Some(_) => false,
                    };
                    if replace {
                        best = Some((r, *tid));
                    }
                }
            }
        }

        let (route, route_tenant) =
            best.ok_or_else(|| route_not_found(format!("no route matches {method} {path}")))?;

        let route_eff = self.build_effective_route(&route, &upstream_eff, &bindings);
        let _ = route_tenant;
        Ok(ResolvedTarget {
            upstream: upstream_eff,
            route: route_eff,
        })
    }

    /// The tenant chain (descendant → root) for `tenant_id`, fabricated from
    /// the tenant resolver.
    async fn tenant_chain(
        &self,
        ctx: &toolkit_security::SecurityContext,
        tenant_id: Uuid,
    ) -> Result<Vec<Uuid>, DomainError> {
        let resp = self
            .tenant_resolver
            .get_ancestors(
                ctx,
                TenantId(tenant_id),
                &tenant_resolver_sdk::GetAncestorsOptions::default(),
            )
            .await
            .map_err(|e| map_resolver_error(&e))?;
        let mut chain = vec![tenant_id];
        chain.extend(resp.ancestors.iter().map(|t| t.id.0));
        Ok(chain)
    }

    /// All upstreams with `alias` along the chain, ordered descendant → root.
    /// Disabled entries are included: resolution rejects the closest disabled
    /// binding with `LinkUnavailable` rather than silently falling through to
    /// an ancestor.
    fn upstream_bindings(&self, chain: &[Uuid], alias: &str) -> Vec<Upstream> {
        let alias = normalize_alias(alias);
        let mut out = Vec::new();
        for tid in chain {
            if let Some(u) = self.repos.upstreams.get_by_alias(*tid, &alias) {
                out.push(u);
            }
        }
        out
    }

    /// Merge a (possibly empty) binding list — root → descendant order —
    /// into an effective upstream.
    #[allow(clippy::unused_self)] // kept as a method for call-site uniformity
    fn build_effective_upstream(&self, bindings: &[Upstream], alias: &str) -> EffectiveUpstream {
        // bindings are descendant → root; flip for root → descendant merges.
        let ord: Vec<&Upstream> = bindings.iter().rev().collect();
        // Callers reject empty binding lists before resolving, so indexing is
        // safe (same panic would be unreachable).
        let selected = &bindings[0];

        // Header overrides merge closest-first (descendant → root), so the
        // closest binding's headers are the source of truth.
        let headers = bindings
            .iter()
            .find_map(|u| u.headers.clone())
            .unwrap_or_default();
        let auth = merge_auth(&ord);
        let plugins = merge_plugins(&ord, None);
        let rate_limit = merge_rate_limits(&ord);
        let cors = merge_cors(&ord);
        let alias_is_common_suffix = is_common_suffix_alias(&selected.server.endpoints);

        EffectiveUpstream {
            id: selected.id.unwrap_or_default(),
            tenant_id: selected.tenant_id.unwrap_or_default(),
            alias: alias.to_owned(),
            enabled: selected.enabled,
            protocol: selected.protocol.clone(),
            server: selected.server.clone(),
            auth,
            headers,
            plugins,
            rate_limit,
            cors,
            alias_is_common_suffix,
        }
    }

    /// Merge the route-level config into the effective route.
    #[allow(clippy::unused_self)] // kept as a method for call-site uniformity
    fn build_effective_route(
        &self,
        route: &Route,
        upstream_eff: &EffectiveUpstream,
        bindings: &[Upstream],
    ) -> EffectiveRoute {
        let ord: Vec<&Upstream> = bindings.iter().rev().collect();
        let plugins = merge_plugins(&ord, route.plugins.as_ref());
        let rate_limit =
            merge_route_rate(upstream_eff.rate_limit.as_ref(), route.rate_limit.as_ref());
        // Route-level CORS config does not exist in the DTO; the effective
        // route inherits the merged upstream CORS configuration.
        let cors = merge_route_cors(upstream_eff.cors.as_ref(), None);

        EffectiveRoute {
            id: route.id.unwrap_or_default(),
            upstream_id: route.upstream_id,
            r#match: route.r#match.clone(),
            enabled: route.enabled,
            plugins,
            rate_limit,
            cors,
        }
    }

    // ------------------------------------------------------------------
    // Validation
    // ------------------------------------------------------------------

    #[allow(clippy::result_large_err)] // DomainError is a broad domain error enum; boxing would ripple through every caller
    fn validate_upstream(&self, _tenant_id: Uuid, input: &Upstream) -> Result<(), DomainError> {
        if input.server.endpoints.is_empty() {
            return Err(validation_err("upstream requires at least one endpoint"));
        }
        if input.protocol != gts_helpers::HTTP_PROTOCOL_ID
            && input.protocol != gts_helpers::GRPC_PROTOCOL_ID
        {
            return Err(validation_err(format!(
                "unsupported protocol '{}'",
                input.protocol
            )));
        }
        if input.is_grpc() {
            return Err(validation_err(
                "gRPC upstreams are catalog-only; no gRPC proxy code path is implemented",
            ));
        }

        let mut seen: Option<(EndpointScheme, u16)> = None;
        for ep in &input.server.endpoints {
            self.validate_endpoint(ep)?;
            if let Some((s, p)) = seen
                && (s != ep.scheme || p != ep.port)
            {
                return Err(validation_err(
                    "all endpoints of an upstream must share the same scheme and port",
                ));
            }
            seen = Some((ep.scheme, ep.port));
        }

        // The auth plugin must be a resolvable builtin. Custom (Starlark)
        // plugins can be created but not executed in this implementation, so
        // binding one would only surface a 503 at request time — reject at
        // validation instead.
        if let Some(auth) = &input.auth
            && let Some(plugin_type) = auth.plugin_type.as_deref()
            && self.auth_registry.resolve(plugin_type).is_none()
        {
            return Err(validation_err(format!(
                "auth plugin type '{plugin_type}' is not executable (only built-in plugins can be bound)"
            )));
        }

        // Plugin bindings must resolve to a builtin guard or transform.
        if let Some(plugins) = &input.plugins {
            for b in &plugins.items {
                if self.guard_registry.resolve(&b.plugin_ref).is_none()
                    && self.transform_registry.resolve(&b.plugin_ref).is_none()
                {
                    return Err(validation_err(format!(
                        "plugin binding '{}' is not executable (only built-in plugins can be bound)",
                        b.plugin_ref
                    )));
                }
            }
        }

        if let Some(rl) = &input.rate_limit {
            if rl.sustained.rate == 0 {
                return Err(validation_err("rate limit sustained.rate must be >= 1"));
            }
            if rl.cost == 0 {
                return Err(validation_err("rate limit cost must be >= 1"));
            }
            if rl.burst.as_ref().is_some_and(|b| b.capacity == 0) {
                return Err(validation_err("rate limit burst.capacity must be >= 1"));
            }
        }

        if let Some(cors) = &input.cors
            && let Some(msg) = cors.validation_error()
        {
            return Err(validation_err(msg));
        }

        Ok(())
    }

    #[allow(clippy::result_large_err)] // DomainError is a broad domain error enum; boxing would ripple through every caller
    fn validate_endpoint(&self, ep: &Endpoint) -> Result<(), DomainError> {
        let host = ep.normalized_host();
        let ip = is_ipv4(&host) || is_ipv6_candidate(&host);
        if !ip && !is_valid_hostname(&host) {
            return Err(validation_err(format!(
                "endpoint host '{}' is not a valid hostname or IP",
                ep.host
            )));
        }
        if ep.port == 0 {
            return Err(validation_err("endpoint port must be between 1 and 65535"));
        }
        if ep.scheme == EndpointScheme::Http && !self.config.allow_http_upstream {
            return Err(validation_err(
                "http endpoints are disabled (allow_http_upstream=false)",
            ));
        }
        if ep.scheme == EndpointScheme::Http && self.config.ssrf_policy.enabled {
            return Err(validation_err(
                "ssrf_policy blocks plaintext http endpoints",
            ));
        }
        if ep.scheme == EndpointScheme::Grpc {
            return Err(validation_err(
                "gRPC endpoints are catalog-only; no gRPC proxy code path is implemented",
            ));
        }
        Ok(())
    }

    #[allow(clippy::result_large_err)] // DomainError is a broad domain error enum; boxing would ripple through every caller
    fn validate_route(&self, tenant_id: Uuid, input: &Route) -> Result<(), DomainError> {
        // Upstream must exist and belong to this tenant.
        if self
            .repos
            .upstreams
            .get(tenant_id, input.upstream_id)
            .is_none()
        {
            return Err(validation_err(format!(
                "upstream_id {} does not exist for this tenant",
                input.upstream_id
            )));
        }
        match &input.r#match {
            MatchConfig::Http { http } => {
                if http.path.is_empty() || !http.path.starts_with('/') {
                    return Err(validation_err("http route path must start with '/'"));
                }
                if http.methods.is_empty() {
                    return Err(validation_err("http route requires at least one method"));
                }
                for m in &http.methods {
                    if !is_valid_http_method(m) {
                        return Err(validation_err(format!("invalid HTTP method '{m}'")));
                    }
                }
            }
            MatchConfig::Grpc { .. } => {
                return Err(validation_err(
                    "gRPC routes are catalog-only; no gRPC proxy code path is implemented",
                ));
            }
        }
        // Match-rule uniqueness within the upstream.
        for existing in self.repos.routes.list(tenant_id, Some(input.upstream_id)) {
            if existing.id == input.id {
                continue;
            }
            if routes_conflict(&existing, input) {
                return Err(route_conflict_err(
                    "a route with the same path and an overlapping method already exists on this upstream",
                    &input.upstream_id,
                ));
            }
        }
        Ok(())
    }

    // ------------------------------------------------------------------
    // Ancestor bind enforcement
    // ------------------------------------------------------------------

    /// Enforce ancestor bind constraints: an ancestor binding with the same
    /// alias whose fields are `enforce` cannot be overridden by `candidate`.
    async fn enforce_ancestor_bindings(
        &self,
        ctx: &toolkit_security::SecurityContext,
        tenant_id: Uuid,
        alias: &str,
        candidate: &Upstream,
    ) -> Result<(), DomainError> {
        let chain = self.tenant_chain(ctx, tenant_id).await?;
        let normalized = normalize_alias(alias);
        // Walk chain excluding the tenant itself, descendant → root (i.e. the
        // ancestors list is direct-parent → root).
        for tid in &chain[1..] {
            if let Some(anc) = self.repos.upstreams.get_by_alias(*tid, &normalized) {
                if !anc.enabled {
                    continue;
                }
                // enforce blocks overrides
                if let Some(a) = anc.auth.as_ref()
                    && a.sharing == SharingMode::Enforce
                    && candidate.auth.is_some()
                {
                    return Err(validation_err(
                        "cannot override ancestor enforced auth config",
                    ));
                }
                if let Some(r) = anc.rate_limit.as_ref()
                    && r.sharing == SharingMode::Enforce
                    && candidate.rate_limit.is_some()
                {
                    return Err(validation_err(
                        "cannot override ancestor enforced rate limit",
                    ));
                }
                if let Some(c) = anc.cors.as_ref()
                    && c.sharing == SharingMode::Enforce
                    && candidate.cors.is_some()
                {
                    return Err(validation_err(
                        "cannot override ancestor enforced cors config",
                    ));
                }
                if let Some(p) = anc.plugins.as_ref()
                    && p.sharing == SharingMode::Enforce
                    && candidate.plugins.is_some()
                {
                    return Err(validation_err(
                        "cannot override ancestor enforced plugin chain",
                    ));
                }
            }
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Pure helpers (kept pub for reuse by the data plane / tests)
// ---------------------------------------------------------------------------

/// Normalize an alias: lowercase + trim + strip trailing dot.
#[must_use]
pub fn normalize_alias(alias: &str) -> String {
    let mut out = alias.trim().to_ascii_lowercase();
    if out.len() > 1 && out.ends_with('.') {
        out.pop();
    }
    out
}

/// Enforce the alias-update matrix (DESIGN §3.5).
#[allow(clippy::result_large_err)] // DomainError is a broad domain error enum; boxing would ripple through every caller
fn enforce_alias_update(
    old_endpoints: &[Endpoint],
    old_alias: &str,
    new_endpoints: &[Endpoint],
    requested_alias: Option<&str>,
) -> Result<String, DomainError> {
    let old_derived = match derive_alias(old_endpoints) {
        AliasDecision::Derived(d) => Some(d),
        AliasDecision::ExplicitRequired => None,
    };
    let new_derived = match derive_alias(new_endpoints) {
        AliasDecision::Derived(d) => Some(d),
        AliasDecision::ExplicitRequired => None,
    };
    let endpoints_changed = old_endpoints != new_endpoints;

    if !endpoints_changed {
        if let Some(r) = requested_alias
            && normalize_alias(r) != old_alias
        {
            return Err(validation_err(
                "alias override not allowed without an endpoint change",
            ));
        }
        return Ok(old_alias.to_owned());
    }

    match (old_derived, new_derived) {
        (Some(_), Some(d)) => {
            if d == old_alias {
                Ok(d)
            } else {
                Err(validation_err(
                    "endpoint change would alter the derived alias; delete and re-create the upstream",
                ))
            }
        }
        (Some(_), None) => Err(validation_err(
            "cannot change endpoints from derivable to non-derivable (would drop the alias)",
        )),
        (None, Some(d)) => {
            if d == old_alias {
                Ok(d)
            } else {
                Err(validation_err(
                    "derived alias for the new endpoints differs from the existing alias; delete and re-create",
                ))
            }
        }
        (None, None) => {
            if let Some(r) = requested_alias
                && normalize_alias(r) != old_alias
            {
                return Err(validation_err(
                    "a differing user-provided alias is not accepted",
                ));
            }
            Ok(old_alias.to_owned())
        }
    }
}

/// Whether the derived alias for these endpoints stems from a multi-host
/// common suffix (which makes `X-OAGW-Target-Host` mandatory).
#[must_use]
pub fn is_common_suffix_alias(endpoints: &[Endpoint]) -> bool {
    let hosts: Vec<String> = endpoints.iter().map(Endpoint::normalized_host).collect();
    let distinct: std::collections::HashSet<&str> = hosts.iter().map(String::as_str).collect();
    distinct.len() > 1 && matches!(derive_alias(endpoints), AliasDecision::Derived(_))
}

/// Whether `candidate` conflicts with `existing` (same path + overlapping
/// methods, both enabled).
#[must_use]
pub fn routes_conflict(existing: &Route, candidate: &Route) -> bool {
    let (Some(a), Some(b)) = (existing.r#match.as_http(), candidate.r#match.as_http()) else {
        return true;
    };
    if normalize_path(&a.path) != normalize_path(&b.path) {
        return false;
    }
    if !existing.enabled || !candidate.enabled {
        return false;
    }
    methods_overlap(&a.methods, &b.methods)
}

fn methods_overlap(a: &[String], b: &[String]) -> bool {
    a.iter()
        .any(|x| b.iter().any(|y| x.eq_ignore_ascii_case(y)))
}

/// Normalized path for comparison (root stays "/", trailing slashes trimmed).
#[must_use]
pub fn normalize_path(path: &str) -> String {
    let t = path.trim_end_matches('/');
    if t.is_empty() {
        "/".to_owned()
    } else {
        t.to_owned()
    }
}

/// Best-effort match of a route for a request method + path. Route paths are
/// prefix matches (longest-prefix selection happens in the caller).
#[must_use]
pub fn route_matches(route: &Route, method: &str, path: &str) -> bool {
    if !route.enabled {
        return false;
    }
    let Some(http) = route.r#match.as_http() else {
        return false;
    };
    if !http.methods.iter().any(|m| m.eq_ignore_ascii_case(method)) {
        return false;
    }
    match http.path_suffix_mode {
        PathSuffixMode::Disabled => normalize_path(&http.path) == normalize_path(path),
        PathSuffixMode::Append => {
            let p = normalize_path(&http.path);
            let q = normalize_path(path);
            // A root `p = "/"` is a catch-all prefix over every request path
            // (the naive `q.starts_with("{p}/")` bound would require `//`).
            q == p || p == "/" || q.starts_with(&format!("{p}/"))
        }
    }
}

fn route_path_len(route: &Route) -> usize {
    route
        .r#match
        .as_http()
        .map_or(0, |h| normalize_path(&h.path).len())
}

fn is_valid_http_method(method: &str) -> bool {
    // The method set the route schema admits (route.v1.schema.json `methods`
    // enum); the proxy itself accepts any verb on the wildcard route.
    let allowed = ["GET", "POST", "PUT", "DELETE", "PATCH"];
    allowed.iter().any(|a| a.eq_ignore_ascii_case(method))
}

fn validation_err(detail: impl Into<String>) -> DomainError {
    DomainError::validation(detail)
}

fn route_conflict_err(detail: &str, upstream_id: &Uuid) -> DomainError {
    DomainError::RouteConflict {
        detail: detail.to_owned(),
        context: Some(ProblemContext {
            upstream_id: Some(gts_helpers::upstream_resource_id(*upstream_id)),
            ..ProblemContext::new()
        }),
    }
}

fn route_not_found(detail: String) -> DomainError {
    DomainError::RouteNotFound {
        detail,
        context: None,
    }
}

fn link_unavailable(detail: String) -> DomainError {
    DomainError::LinkUnavailable {
        detail,
        context: None,
    }
}

fn map_resolver_error(e: &TenantResolverError) -> DomainError {
    DomainError::Internal {
        detail: format!("tenant resolver error: {e}"),
        source: None,
    }
}

// --- merge helpers --------------------------------------------------------

fn ordered_union(items: impl Iterator<Item = String>) -> Vec<String> {
    let mut out = Vec::new();
    for i in items {
        if !out.contains(&i) {
            out.push(i);
        }
    }
    out
}

fn merge_auth(ord: &[&Upstream]) -> Option<AuthConfig> {
    let mut current: Option<AuthConfig> = None;
    let selected = ord.len().saturating_sub(1);
    for (i, u) in ord.iter().enumerate() {
        let Some(a) = u.auth.as_ref() else { continue };
        // A `private` ancestor auth config does not propagate; only the
        // selected (closest) binding's private config applies.
        if i != selected && a.sharing == SharingMode::Private {
            continue;
        }
        current = match current {
            Some(cur) if cur.sharing == SharingMode::Enforce => Some(cur),
            _ => Some(a.clone()),
        };
    }
    current
}

fn merge_plugins(ord: &[&Upstream], route_plugins: Option<&PluginsConfig>) -> PluginsConfig {
    let mut items = Vec::new();
    let mut sharing = SharingMode::Private;
    let selected = ord.len().saturating_sub(1);
    for (i, u) in ord.iter().enumerate() {
        if let Some(p) = u.plugins.as_ref() {
            // A `private` ancestor chain does not propagate; only the
            // selected (closest) binding's private chain applies.
            if i != selected && p.sharing == SharingMode::Private {
                continue;
            }
            sharing = p.sharing;
            items.extend(p.items.clone());
        }
    }
    if let Some(rp) = route_plugins {
        items.extend(rp.items.clone());
    }
    PluginsConfig { sharing, items }
}

fn merge_rate_limits(ord: &[&Upstream]) -> Option<RateLimitConfig> {
    // The closest binding's limit plus every non-private ancestor — the
    // per-field minimum wins (ADR-0003), with the qualitative fields
    // (strategy / scoping / headers) riding on the closest config.
    let mut candidates: Vec<&RateLimitConfig> = Vec::new();
    if let Some(last) = ord.last()
        && let Some(r) = last.rate_limit.as_ref()
    {
        candidates.push(r);
    }
    for u in ord {
        if let Some(r) = u.rate_limit.as_ref()
            && r.sharing != SharingMode::Private
        {
            candidates.push(r);
        }
    }
    let &first = candidates.first()?;
    let mut effective = first.clone();
    for r in candidates.iter().skip(1) {
        effective = tighter_rate(&effective, r);
    }
    if let Some(last) = ord.last()
        && let Some(selected) = last.rate_limit.as_ref()
    {
        effective.strategy = selected.strategy;
        effective.response_headers = selected.response_headers;
        effective.sharing = selected.sharing;
        effective.scope = selected.scope;
        effective.algorithm = selected.algorithm;
    }
    Some(effective)
}

/// Merge two rate-limit configs field by field: the tighter (more
/// restrictive) value of every quantitative field wins (ADR-0003).
/// Qualitative fields (sharing / strategy / scope / algorithm /
/// `response_headers`) carry over from `a` unchanged.
fn tighter_rate(a: &RateLimitConfig, b: &RateLimitConfig) -> RateLimitConfig {
    let burst = if a.burst.is_some() || b.burst.is_some() {
        Some(BurstSpec {
            capacity: a.burst_capacity().min(b.burst_capacity()),
        })
    } else {
        None
    };
    RateLimitConfig {
        sharing: a.sharing,
        algorithm: a.algorithm,
        sustained: tighter_spec(a.sustained, b.sustained),
        burst,
        scope: a.scope,
        strategy: a.strategy,
        cost: a.cost.min(b.cost),
        response_headers: a.response_headers,
    }
}

/// The stricter of two rate specs: the lower tokens-per-second rate wins
/// (rates normalize across windows via exact integer cross-multiplication).
fn tighter_spec(a: RateSpec, b: RateSpec) -> RateSpec {
    let a_norm = u128::from(a.rate) * u128::from(b.window.as_secs());
    let b_norm = u128::from(b.rate) * u128::from(a.window.as_secs());
    if b_norm < a_norm { b } else { a }
}

fn merge_cors(ord: &[&Upstream]) -> Option<CorsConfig> {
    let mut current: Option<CorsConfig> = None;
    let selected = ord.len().saturating_sub(1);
    for (i, u) in ord.iter().enumerate() {
        let Some(c) = u.cors.as_ref() else { continue };
        // A `private` ancestor CORS config does not propagate at all; only
        // the selected (closest) binding's private config clobbers the
        // merged result.
        if i != selected && c.sharing == SharingMode::Private {
            continue;
        }
        current = Some(match current {
            None => c.clone(),
            Some(cur) => {
                if cur.sharing == SharingMode::Enforce {
                    cors_union(&cur, c)
                } else if c.sharing == SharingMode::Private {
                    c.clone()
                } else {
                    cors_union(&cur, c)
                }
            }
        });
    }
    current
}

fn cors_union(a: &CorsConfig, b: &CorsConfig) -> CorsConfig {
    let allow_credentials = a.allow_credentials || b.allow_credentials;
    let mut allowed_origins = ordered_union(
        a.allowed_origins
            .iter()
            .chain(b.allowed_origins.iter())
            .cloned(),
    );
    if allow_credentials {
        // A wildcard origin cannot be combined with credentialed requests
        // (browsers reject the pair); drop the wildcard from the union.
        allowed_origins.retain(|o| o != "*");
    }
    CorsConfig {
        sharing: b.sharing,
        enabled: a.enabled || b.enabled,
        allowed_origins,
        allowed_methods: ordered_union(
            a.allowed_methods
                .iter()
                .chain(b.allowed_methods.iter())
                .cloned(),
        ),
        expose_headers: ordered_union(
            a.expose_headers
                .iter()
                .chain(b.expose_headers.iter())
                .cloned(),
        ),
        allow_credentials,
    }
}

fn merge_route_rate(
    upstream: Option<&RateLimitConfig>,
    route: Option<&RateLimitConfig>,
) -> Option<RateLimitConfig> {
    match (upstream, route) {
        // Per-field min; the qualitative fields ride on the upstream config
        // (`tighter_rate` carries them from `a`), keeping `u`'s sharing.
        (Some(u), Some(r)) => Some(tighter_rate(u, r)),
        (Some(u), None) => Some(u.clone()),
        (None, Some(r)) => Some(r.clone()),
        (None, None) => None,
    }
}

fn merge_route_cors(
    upstream: Option<&CorsConfig>,
    route: Option<&CorsConfig>,
) -> Option<CorsConfig> {
    match (upstream, route) {
        (Some(u), Some(r)) => Some(cors_union(u, r)),
        (Some(u), None) => Some(u.clone()),
        (None, Some(r)) => Some(r.clone()),
        (None, None) => None,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use std::sync::Arc;

    use tenant_resolver_sdk::{
        GetAncestorsOptions, GetAncestorsResponse, GetDescendantsOptions, GetDescendantsResponse,
        GetTenantsOptions, IsAncestorOptions, TenantId, TenantInfo, TenantRef, TenantStatus,
    };

    use super::*;
    use crate::domain::dto::{
        Endpoint, EndpointScheme, HttpMatch, MatchConfig, PluginBinding, PluginsConfig,
        ServerConfig,
    };

    fn ctx_for(tenant: Uuid) -> toolkit_security::SecurityContext {
        toolkit_security::SecurityContext::builder()
            .subject_id(Uuid::new_v4())
            .subject_tenant_id(tenant)
            .build()
            .unwrap()
    }

    /// Fake tenant resolver with a configurable ancestor chain.
    struct FakeResolver {
        ancestors: Vec<Uuid>,
    }

    #[async_trait::async_trait]
    impl TenantResolverClient for FakeResolver {
        async fn get_tenant(
            &self,
            _ctx: &toolkit_security::SecurityContext,
            _id: TenantId,
        ) -> Result<TenantInfo, TenantResolverError> {
            unimplemented!()
        }
        async fn get_root_tenant(
            &self,
            _ctx: &toolkit_security::SecurityContext,
        ) -> Result<TenantInfo, TenantResolverError> {
            unimplemented!()
        }
        async fn get_tenants(
            &self,
            _ctx: &toolkit_security::SecurityContext,
            _ids: &[TenantId],
            _options: &GetTenantsOptions,
        ) -> Result<Vec<TenantInfo>, TenantResolverError> {
            unimplemented!()
        }
        async fn get_ancestors(
            &self,
            _ctx: &toolkit_security::SecurityContext,
            _id: TenantId,
            _options: &GetAncestorsOptions,
        ) -> Result<GetAncestorsResponse, TenantResolverError> {
            Ok(GetAncestorsResponse {
                tenant: TenantRef {
                    id: TenantId(Uuid::default()),
                    status: TenantStatus::Active,
                    tenant_type: None,
                    parent_id: None,
                    self_managed: false,
                },
                ancestors: self
                    .ancestors
                    .iter()
                    .map(|id| TenantRef {
                        id: TenantId(*id),
                        status: TenantStatus::Active,
                        tenant_type: None,
                        parent_id: None,
                        self_managed: false,
                    })
                    .collect(),
            })
        }
        async fn get_descendants(
            &self,
            _ctx: &toolkit_security::SecurityContext,
            _id: TenantId,
            _options: &GetDescendantsOptions,
        ) -> Result<GetDescendantsResponse, TenantResolverError> {
            unimplemented!()
        }
        async fn is_ancestor(
            &self,
            _ctx: &toolkit_security::SecurityContext,
            _parent_id: TenantId,
            _descendant_id: TenantId,
            _options: &IsAncestorOptions,
        ) -> Result<bool, TenantResolverError> {
            unimplemented!()
        }
    }

    fn service(resolver: FakeResolver) -> ControlPlaneService {
        let config = OagwConfig::default();
        let repos = MemoryRepos::new();
        let auth = Arc::new(AuthPluginRegistry::new(repos.plugins.clone()));
        let guard = Arc::new(GuardPluginRegistry::new(repos.plugins.clone()));
        let transform = Arc::new(TransformPluginRegistry::new(repos.plugins.clone()));
        ControlPlaneService::new(repos, Arc::new(resolver), auth, guard, transform, config)
    }

    fn upstream_input(host: &str, port: u16) -> Upstream {
        Upstream {
            server: ServerConfig {
                endpoints: vec![Endpoint {
                    scheme: EndpointScheme::Https,
                    host: host.to_owned(),
                    port,
                }],
            },
            ..Upstream::default()
        }
    }

    #[tokio::test]
    async fn create_upstream_derives_alias() {
        let tenant = Uuid::new_v4();
        let svc = service(FakeResolver { ancestors: vec![] });
        let input = upstream_input("api.example.com", 443);
        let created = svc
            .create_upstream(&ctx_for(tenant), tenant, input)
            .await
            .unwrap();
        assert_eq!(created.alias.as_deref(), Some("api.example.com"));
        assert!(created.id.is_some());
        assert_eq!(created.tenant_id, Some(tenant));
    }

    #[tokio::test]
    async fn create_upstream_duplicate_alias_conflicts() {
        let tenant = Uuid::new_v4();
        let svc = service(FakeResolver { ancestors: vec![] });
        let ctx = ctx_for(tenant);
        svc.create_upstream(&ctx, tenant, upstream_input("api.example.com", 443))
            .await
            .unwrap();
        let err = svc
            .create_upstream(&ctx, tenant, upstream_input("api.example.com", 443))
            .await
            .unwrap_err();
        assert_eq!(err.problem_info().status, 409);
    }

    #[tokio::test]
    async fn ip_endpoints_require_explicit_alias() {
        let tenant = Uuid::new_v4();
        let svc = service(FakeResolver { ancestors: vec![] });
        let err = svc
            .create_upstream(&ctx_for(tenant), tenant, upstream_input("192.168.1.5", 443))
            .await
            .unwrap_err();
        assert_eq!(err.problem_info().status, 400);
    }

    #[tokio::test]
    async fn route_conflict_returns_409() {
        let tenant = Uuid::new_v4();
        let svc = service(FakeResolver { ancestors: vec![] });
        let ctx = ctx_for(tenant);
        let up = svc
            .create_upstream(&ctx, tenant, upstream_input("api.example.com", 443))
            .await
            .unwrap();
        let up_id = up.id.unwrap();

        let r1 = Route {
            upstream_id: up_id,
            r#match: MatchConfig::Http {
                http: HttpMatch {
                    methods: vec!["GET".into()],
                    path: "/v1".into(),
                    ..HttpMatch::default()
                },
            },
            ..Route::default()
        };
        svc.create_route(tenant, r1).unwrap();

        let r2 = Route {
            upstream_id: up_id,
            r#match: MatchConfig::Http {
                http: HttpMatch {
                    methods: vec!["POST".into()],
                    path: "/v1".into(),
                    ..HttpMatch::default()
                },
            },
            ..Route::default()
        };
        svc.create_route(tenant, r2).unwrap();

        let r3 = Route {
            upstream_id: up_id,
            r#match: MatchConfig::Http {
                http: HttpMatch {
                    methods: vec!["GET".into()],
                    path: "/v1".into(),
                    ..HttpMatch::default()
                },
            },
            ..Route::default()
        };
        let err = svc.create_route(tenant, r3).unwrap_err();
        assert_eq!(err.problem_info().status, 409);
    }

    #[tokio::test]
    async fn alias_update_is_immutable() {
        let tenant = Uuid::new_v4();
        let svc = service(FakeResolver { ancestors: vec![] });
        let ctx = ctx_for(tenant);
        let up = svc
            .create_upstream(&ctx, tenant, upstream_input("api.example.com", 443))
            .await
            .unwrap();
        let id = up.id.unwrap();

        // Endpoint change that would alter the alias → rejected.
        let err = svc
            .update_upstream(&ctx, tenant, id, upstream_input("other.example.com", 443))
            .await
            .unwrap_err();
        assert_eq!(err.problem_info().status, 400);

        // Same endpoints, same alias → tolerated.
        let ok = svc
            .update_upstream(&ctx, tenant, id, upstream_input("api.example.com", 443))
            .await
            .unwrap();
        assert_eq!(ok.alias.as_deref(), Some("api.example.com"));
    }

    /// Multi-endpoint upstream input (common-suffix alias derivation).
    fn multi_upstream(hosts: &[&str], port: u16) -> Upstream {
        Upstream {
            server: ServerConfig {
                endpoints: hosts
                    .iter()
                    .map(|h| Endpoint {
                        scheme: EndpointScheme::Https,
                        host: h.to_string(),
                        port,
                    })
                    .collect(),
            },
            ..Upstream::default()
        }
    }

    #[tokio::test]
    async fn resolve_alias_shadowing_closest_wins() {
        let parent = Uuid::new_v4();
        let child = Uuid::new_v4();
        let svc = service(FakeResolver {
            ancestors: vec![parent],
        });
        let pctx = ctx_for(parent);
        svc.create_upstream(
            &pctx,
            parent,
            multi_upstream(&["us.vendor.com", "ny.vendor.com"], 443),
        )
        .await
        .unwrap();

        let cctx = ctx_for(child);
        let shadow = svc
            .create_upstream(
                &cctx,
                child,
                multi_upstream(&["eu.vendor.com", "de.vendor.com"], 443),
            )
            .await
            .unwrap();
        assert_eq!(shadow.alias.as_deref(), Some("vendor.com"));

        let resolved = svc
            .resolve_upstream(&cctx, child, "vendor.com")
            .await
            .unwrap();
        assert_eq!(resolved.id, shadow.id.unwrap());
        assert_eq!(resolved.server.endpoints[0].host, "eu.vendor.com");
    }

    #[tokio::test]
    async fn delete_plugin_in_use_returns_plugin_in_use() {
        let tenant = Uuid::new_v4();
        let svc = service(FakeResolver { ancestors: vec![] });
        let plugin = Plugin {
            plugin_type: "guard".into(),
            name: "my-guard".into(),
            ..Plugin::default()
        };
        let created = svc.create_plugin(tenant, plugin).unwrap();
        let gts = created.gts_id();

        let ctx = ctx_for(tenant);
        let up = svc
            .create_upstream(&ctx, tenant, upstream_input("api.example.com", 443))
            .await
            .unwrap();
        let mut up2 = up;
        up2.plugins = Some(PluginsConfig {
            items: vec![PluginBinding::new(gts)],
            ..PluginsConfig::default()
        });
        svc.repos.upstreams.replace(up2);

        let err = svc.delete_plugin(tenant, created.id.unwrap()).unwrap_err();
        let info = err.problem_info();
        assert_eq!(info.status, 409);
        assert_eq!(info.type_id, gts_helpers::ERROR_PLUGIN_IN_USE);
    }
}
