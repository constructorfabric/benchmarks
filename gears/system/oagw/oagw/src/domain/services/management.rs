//! Control Plane — configuration ownership and proxy-target resolution.
//!
//! Management CRUD is strictly tenant-scoped: an ancestor's resources are
//! invisible (404) through this API. Proxy-time resolution is the one place
//! that walks the tenant chain, because alias shadowing and configuration
//! inheritance are defined there (`docs/DESIGN.md` §"Tenant Scoping").

use async_trait::async_trait;
use std::sync::Arc;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::domain::alias;
use crate::domain::cors;
use crate::domain::error::{DomainError, DomainResult};
use crate::domain::gts_helpers;
use crate::domain::merge;
use crate::domain::model::{
    AuthConfig, CorsConfig, HeadersConfig, MatchConfig, PathSuffixMode, Plugin, PluginKind,
    PluginPhase, PluginsConfig, RateLimitConfig, Route, ServerConfig, Upstream,
};
use crate::domain::repo::{PluginRepository, RouteRepository, UpstreamRepository};
use crate::domain::services::tenancy::{TenantHierarchy, resolution_chain};

/// HTTP methods a route may declare
/// (`docs/schemas/route.v1.schema.json` `http_match.methods`).
pub const ALLOWED_ROUTE_METHODS: [&str; 5] = ["GET", "POST", "PUT", "DELETE", "PATCH"];

/// Write-side payload for an upstream. Optional members are cleared on
/// replace, matching the documented full-replacement PUT semantics.
#[derive(Debug, Clone)]
pub struct UpstreamSpec {
    /// Operator-supplied alias; only honoured for non-derivable pools.
    pub alias: Option<String>,
    /// Whether proxy traffic is accepted; defaults to `true`.
    pub enabled: Option<bool>,
    /// Discovery tags.
    pub tags: Vec<String>,
    /// Endpoint pool.
    pub server: ServerConfig,
    /// Protocol GTS identifier.
    pub protocol: String,
    /// Outbound authentication.
    pub auth: Option<AuthConfig>,
    /// Header transformation rules.
    pub headers: Option<HeadersConfig>,
    /// Plugin chain.
    pub plugins: Option<PluginsConfig>,
    /// Rate limit.
    pub rate_limit: Option<RateLimitConfig>,
    /// CORS policy.
    pub cors: Option<CorsConfig>,
}

/// Write-side payload for a route.
#[derive(Debug, Clone)]
pub struct RouteSpec {
    /// Owning upstream. Immutable — ignored on replace.
    pub upstream_id: Option<Uuid>,
    /// Whether the route participates in matching; defaults to `true`.
    pub enabled: Option<bool>,
    /// Tie-breaker for equally specific paths.
    pub priority: Option<i32>,
    /// Discovery tags.
    pub tags: Vec<String>,
    /// Match rules.
    pub match_config: MatchConfig,
    /// Plugin chain.
    pub plugins: Option<PluginsConfig>,
    /// Rate limit.
    pub rate_limit: Option<RateLimitConfig>,
    /// CORS policy.
    pub cors: Option<CorsConfig>,
}

/// Write-side payload for a custom plugin definition.
#[derive(Debug, Clone)]
pub struct PluginSpec {
    /// Unique-per-tenant name.
    pub name: String,
    /// Free-form description.
    pub description: Option<String>,
    /// Which trait the plugin implements.
    pub plugin_type: PluginKind,
    /// Declared lifecycle phases.
    pub phases: Vec<PluginPhase>,
    /// JSON Schema constraining binding configuration.
    pub config_schema: serde_json::Value,
    /// Plugin body.
    pub source_code: String,
}

/// A rate limit to enforce, together with the counter it belongs to.
#[derive(Debug, Clone)]
pub struct RateLimitTarget {
    /// `upstream` or `route` — the counter-key resource type.
    pub resource_type: &'static str,
    /// Identifier of the resource owning the counter.
    pub resource_id: Uuid,
    /// Effective configuration after hierarchical merge.
    pub config: RateLimitConfig,
}

/// Everything the Data Plane needs to execute one proxy request.
#[derive(Debug, Clone)]
pub struct ResolvedTarget {
    /// The closest upstream matching the alias.
    pub upstream: Upstream,
    /// The matched route, when one matched.
    pub route: Option<Route>,
    /// Effective outbound auth after hierarchical merge.
    pub auth: Option<AuthConfig>,
    /// Effective plugin chain: upstream bindings then route bindings.
    pub plugins: PluginsConfig,
    /// Effective header rules.
    pub headers: HeadersConfig,
    /// Effective CORS policy.
    pub cors: Option<CorsConfig>,
    /// Rate limits to enforce, in check order.
    pub rate_limits: Vec<RateLimitTarget>,
    /// Effective discovery tags.
    pub tags: Vec<String>,
}

/// Configuration ownership and proxy-target resolution.
#[async_trait]
pub trait ControlPlaneService: Send + Sync {
    /// Create an upstream for the caller's tenant.
    ///
    /// # Errors
    ///
    /// `400` on validation failure, `409` on alias conflict.
    async fn create_upstream(
        &self,
        ctx: &SecurityContext,
        spec: UpstreamSpec,
    ) -> DomainResult<Upstream>;

    /// Fetch one of the caller's upstreams.
    ///
    /// # Errors
    ///
    /// `404` when absent or owned by another tenant.
    async fn get_upstream(&self, ctx: &SecurityContext, id: Uuid) -> DomainResult<Upstream>;

    /// List the caller's upstreams.
    ///
    /// # Errors
    ///
    /// Propagates storage failures.
    async fn list_upstreams(&self, ctx: &SecurityContext) -> DomainResult<Vec<Upstream>>;

    /// Replace one of the caller's upstreams wholesale.
    ///
    /// # Errors
    ///
    /// `400` on validation failure, `404` when absent.
    async fn replace_upstream(
        &self,
        ctx: &SecurityContext,
        id: Uuid,
        spec: UpstreamSpec,
    ) -> DomainResult<Upstream>;

    /// Delete one of the caller's upstreams and cascade to its routes.
    ///
    /// # Errors
    ///
    /// `404` when absent.
    async fn delete_upstream(&self, ctx: &SecurityContext, id: Uuid) -> DomainResult<()>;

    /// Create a route on one of the caller's upstreams.
    ///
    /// # Errors
    ///
    /// `400` on validation failure, `409` on a duplicate match rule.
    async fn create_route(&self, ctx: &SecurityContext, spec: RouteSpec) -> DomainResult<Route>;

    /// Fetch one of the caller's routes.
    ///
    /// # Errors
    ///
    /// `404` when absent or owned by another tenant.
    async fn get_route(&self, ctx: &SecurityContext, id: Uuid) -> DomainResult<Route>;

    /// List the caller's routes.
    ///
    /// # Errors
    ///
    /// Propagates storage failures.
    async fn list_routes(&self, ctx: &SecurityContext) -> DomainResult<Vec<Route>>;

    /// Replace one of the caller's routes wholesale.
    ///
    /// # Errors
    ///
    /// `400` on validation failure, `404` when absent.
    async fn replace_route(
        &self,
        ctx: &SecurityContext,
        id: Uuid,
        spec: RouteSpec,
    ) -> DomainResult<Route>;

    /// Delete one of the caller's routes.
    ///
    /// # Errors
    ///
    /// `404` when absent.
    async fn delete_route(&self, ctx: &SecurityContext, id: Uuid) -> DomainResult<()>;

    /// Create a custom plugin definition.
    ///
    /// # Errors
    ///
    /// `400` on validation failure, `409` on a duplicate name.
    async fn create_plugin(&self, ctx: &SecurityContext, spec: PluginSpec) -> DomainResult<Plugin>;

    /// Fetch one of the caller's plugin definitions.
    ///
    /// # Errors
    ///
    /// `404` when absent or owned by another tenant.
    async fn get_plugin(&self, ctx: &SecurityContext, id: Uuid) -> DomainResult<Plugin>;

    /// List the caller's plugin definitions.
    ///
    /// # Errors
    ///
    /// Propagates storage failures.
    async fn list_plugins(&self, ctx: &SecurityContext) -> DomainResult<Vec<Plugin>>;

    /// Delete a plugin definition that nothing references.
    ///
    /// # Errors
    ///
    /// `404` when absent, `409` when still bound.
    async fn delete_plugin(&self, ctx: &SecurityContext, id: Uuid) -> DomainResult<()>;

    /// Resolve the alias and route for a proxy request, merging the
    /// hierarchy into one effective configuration.
    ///
    /// # Errors
    ///
    /// `404` when no upstream or route matches, `503` when the upstream —
    /// or any ancestor of it — is disabled.
    async fn resolve_proxy_target(
        &self,
        ctx: &SecurityContext,
        alias: &str,
        method: &str,
        path_suffix: Option<&str>,
    ) -> DomainResult<ResolvedTarget>;

    /// Look up a custom plugin definition by id across tenants, for
    /// proxy-time binding resolution.
    ///
    /// # Errors
    ///
    /// Propagates storage failures.
    async fn resolve_custom_plugin(&self, id: Uuid) -> DomainResult<Option<Plugin>>;
}

/// The shipped Control Plane.
pub struct ControlPlaneServiceImpl {
    upstreams: Arc<dyn UpstreamRepository>,
    routes: Arc<dyn RouteRepository>,
    plugins: Arc<dyn PluginRepository>,
    hierarchy: Arc<dyn TenantHierarchy>,
    /// TTL after which an unlinked plugin becomes collectable.
    plugin_gc_ttl_secs: u64,
    /// Whether the deployment permits plaintext upstream schemes at all.
    /// Only affects *messages* here; the connect-time gate lives in the
    /// Data Plane so that the two layers stay separable.
    allow_http_upstream: bool,
}

impl ControlPlaneServiceImpl {
    /// Wire the Control Plane to its repositories and the tenant hierarchy.
    #[must_use]
    pub fn new(
        upstreams: Arc<dyn UpstreamRepository>,
        routes: Arc<dyn RouteRepository>,
        plugins: Arc<dyn PluginRepository>,
        hierarchy: Arc<dyn TenantHierarchy>,
        plugin_gc_ttl_secs: u64,
        allow_http_upstream: bool,
    ) -> Self {
        Self {
            upstreams,
            routes,
            plugins,
            hierarchy,
            plugin_gc_ttl_secs,
            allow_http_upstream,
        }
    }

    /// Whether the deployment permits plaintext upstream connections.
    #[must_use]
    pub const fn allows_http_upstream(&self) -> bool {
        self.allow_http_upstream
    }

    fn now_epoch_secs() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs())
    }

    /// Validate everything about an upstream that does not depend on
    /// neighbouring rows.
    fn validate_upstream_spec(&self, spec: &UpstreamSpec) -> DomainResult<()> {
        alias::validate_pool(&spec.server.endpoints)?;

        if spec.protocol != gts_helpers::PROTOCOL_HTTP && spec.protocol != gts_helpers::PROTOCOL_GRPC
        {
            return Err(DomainError::validation(format!(
                "unknown protocol '{}'; expected '{}' or '{}'",
                spec.protocol,
                gts_helpers::PROTOCOL_HTTP,
                gts_helpers::PROTOCOL_GRPC
            )));
        }

        for tag in &spec.tags {
            validate_tag(tag)?;
        }

        if let Some(auth) = &spec.auth
            && let Some(plugin_ref) = &auth.plugin_ref
        {
            validate_plugin_ref(plugin_ref, gts_helpers::AUTH_PLUGIN_TYPE, "auth.type")?;
        }

        if let Some(plugins) = &spec.plugins {
            validate_chain(plugins)?;
        }

        if let Some(rate_limit) = &spec.rate_limit {
            validate_rate_limit(rate_limit)?;
        }

        if let Some(cors_cfg) = &spec.cors {
            cors::validate_config(cors_cfg)?;
        }

        Ok(())
    }

    fn validate_route_spec(&self, spec: &RouteSpec, upstream: &Upstream) -> DomainResult<()> {
        match (&spec.match_config.http, &spec.match_config.grpc) {
            (Some(_), Some(_)) | (None, None) => {
                return Err(DomainError::validation(
                    "match must contain exactly one of 'http' or 'grpc'",
                ));
            }
            (Some(http), None) => {
                if !upstream.is_http() {
                    return Err(DomainError::validation(
                        "match.http requires an upstream with the HTTP protocol",
                    ));
                }
                if http.methods.is_empty() {
                    return Err(DomainError::validation(
                        "match.http.methods must list at least one method",
                    ));
                }
                for method in &http.methods {
                    if !ALLOWED_ROUTE_METHODS.contains(&method.as_str()) {
                        return Err(DomainError::validation(format!(
                            "unsupported method '{method}'; expected one of {}",
                            ALLOWED_ROUTE_METHODS.join(", ")
                        )));
                    }
                }
                if http.path.is_empty() {
                    return Err(DomainError::validation(
                        "match.http.path must not be empty",
                    ));
                }
                if !http.path.starts_with('/') {
                    return Err(DomainError::validation(
                        "match.http.path must start with '/'",
                    ));
                }
                if http.path.contains("..") {
                    return Err(DomainError::validation(
                        "match.http.path must not contain '..' segments",
                    ));
                }
            }
            (None, Some(grpc)) => {
                if upstream.is_http() {
                    return Err(DomainError::validation(
                        "match.grpc requires an upstream with the gRPC protocol",
                    ));
                }
                if grpc.service.is_empty() || grpc.method.is_empty() {
                    return Err(DomainError::validation(
                        "match.grpc.service and match.grpc.method must not be empty",
                    ));
                }
            }
        }

        for tag in &spec.tags {
            validate_tag(tag)?;
        }
        if let Some(plugins) = &spec.plugins {
            validate_chain(plugins)?;
        }
        if let Some(rate_limit) = &spec.rate_limit {
            validate_rate_limit(rate_limit)?;
        }
        if let Some(cors_cfg) = &spec.cors {
            cors::validate_config(cors_cfg)?;
        }
        Ok(())
    }

    /// Reject a route whose `(path, priority, method)` collides with another
    /// enabled route on the same upstream.
    async fn ensure_match_unique(
        &self,
        upstream_id: Uuid,
        spec: &RouteSpec,
        exclude: Option<Uuid>,
    ) -> DomainResult<()> {
        let Some(candidate) = &spec.match_config.http else {
            return Ok(());
        };
        if spec.enabled == Some(false) {
            return Ok(());
        }
        let priority = spec.priority.unwrap_or(0);
        for existing in self.routes.list_by_upstream(upstream_id).await? {
            if Some(existing.id) == exclude || !existing.enabled {
                continue;
            }
            let Some(existing_match) = &existing.match_config.http else {
                continue;
            };
            if existing_match.path != candidate.path || existing.priority != priority {
                continue;
            }
            if let Some(method) = existing_match
                .methods
                .iter()
                .find(|m| candidate.methods.contains(m))
            {
                return Err(DomainError::conflict(format!(
                    "a route on this upstream already matches {method} {} at priority {priority}",
                    candidate.path
                )));
            }
        }
        Ok(())
    }

    /// Confirm that every UUID-backed binding resolves to a stored plugin of
    /// the right kind, and mark those plugins as linked.
    async fn bind_plugins(&self, tenant_id: Uuid, chain: &PluginsConfig) -> DomainResult<()> {
        for binding in &chain.items {
            let Some(uuid) = binding.plugin_uuid() else {
                continue;
            };
            let plugin = self
                .plugins
                .get_any(uuid)
                .await?
                .ok_or_else(|| {
                    DomainError::validation(format!(
                        "plugin '{}' is not registered",
                        binding.plugin_ref
                    ))
                })?;
            if plugin.tenant_id != tenant_id {
                return Err(DomainError::validation(format!(
                    "plugin '{}' belongs to another tenant",
                    binding.plugin_ref
                )));
            }
            if let Some(base) = gts_helpers::base_part(&binding.plugin_ref)
                && let Some(kind) = PluginKind::from_base_type(base)
                && kind != plugin.plugin_type
            {
                return Err(DomainError::validation(format!(
                    "plugin '{}' is a {} plugin, not {}",
                    binding.plugin_ref,
                    plugin.plugin_type.as_str(),
                    kind.as_str()
                )));
            }
            self.plugins.set_gc_eligible_at(uuid, None).await?;
        }
        Ok(())
    }

    /// Count references to `plugin_id` across upstreams and routes.
    async fn plugin_references(&self, plugin_id: Uuid) -> DomainResult<(Vec<Uuid>, Vec<Uuid>)> {
        let mut upstream_refs = Vec::new();
        let mut route_refs = Vec::new();

        for upstream in self.upstreams.all().await? {
            let auth_hit = upstream
                .auth
                .as_ref()
                .and_then(|a| a.plugin_ref.as_deref())
                .and_then(gts_helpers::plugin_ref_uuid)
                == Some(plugin_id);
            let chain_hit = upstream
                .plugins
                .items
                .iter()
                .any(|b| b.plugin_uuid() == Some(plugin_id));
            if auth_hit || chain_hit {
                upstream_refs.push(upstream.id);
            }
        }
        for route in self.routes.all().await? {
            if route
                .plugins
                .items
                .iter()
                .any(|b| b.plugin_uuid() == Some(plugin_id))
            {
                route_refs.push(route.id);
            }
        }
        Ok((upstream_refs, route_refs))
    }

    /// Mark every plugin that is no longer referenced as GC-eligible, and
    /// drop the ones whose TTL has elapsed.
    ///
    /// `docs/DESIGN.md` §"Plugin Lifecycle Management" describes this as a
    /// periodic job. The gear declares no `stateful` capability (and so has
    /// no runner), so the sweep is driven from the write paths that can
    /// unlink a plugin — which is precisely when the eligibility marker
    /// needs to change.
    async fn sweep_plugins(&self) -> DomainResult<()> {
        let now = Self::now_epoch_secs();
        for plugin in self.plugins_all().await? {
            let (upstreams, routes) = self.plugin_references(plugin.id).await?;
            let linked = !upstreams.is_empty() || !routes.is_empty();
            match (linked, plugin.gc_eligible_at) {
                (true, Some(_)) => self.plugins.set_gc_eligible_at(plugin.id, None).await?,
                (false, None) => {
                    self.plugins
                        .set_gc_eligible_at(plugin.id, Some(now + self.plugin_gc_ttl_secs))
                        .await?;
                }
                _ => {}
            }
        }
        let collected = self.plugins.collect_garbage(now).await?;
        if collected > 0 {
            tracing::info!(target: "oagw.plugin", collected, "collected unlinked plugins");
        }
        Ok(())
    }

    async fn plugins_all(&self) -> DomainResult<Vec<Plugin>> {
        // The repository is tenant-scoped by design; the sweep needs a
        // cross-tenant view, which every reference check already has via the
        // upstream/route tables. Collect the distinct tenants first.
        let mut tenants: Vec<Uuid> = Vec::new();
        for upstream in self.upstreams.all().await? {
            if !tenants.contains(&upstream.tenant_id) {
                tenants.push(upstream.tenant_id);
            }
        }
        let mut plugins = Vec::new();
        for tenant in tenants {
            plugins.extend(self.plugins.list(tenant).await?);
        }
        Ok(plugins)
    }
}

/// Discovery-tag grammar from the entity schemas: `^[a-z0-9_-]+$`.
fn validate_tag(tag: &str) -> DomainResult<()> {
    if tag.is_empty()
        || !tag
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'_' | b'-'))
    {
        return Err(DomainError::validation(format!(
            "tag must match ^[a-z0-9_-]+$: '{tag}'"
        )));
    }
    Ok(())
}

/// A binding must name a plugin the gateway can actually resolve.
fn validate_plugin_ref(plugin_ref: &str, expected_base: &str, field: &str) -> DomainResult<()> {
    if gts_helpers::CATALOG_ONLY_PLUGIN_IDS.contains(&plugin_ref) {
        return Err(DomainError::validation(format!(
            "{field}: '{plugin_ref}' is a catalog-only identifier with no backing implementation"
        )));
    }
    if gts_helpers::plugin_ref_uuid(plugin_ref).is_some() {
        return Ok(());
    }
    match gts_helpers::base_part(plugin_ref) {
        Some(base) if base == expected_base => Ok(()),
        Some(base) => Err(DomainError::validation(format!(
            "{field}: expected a '{expected_base}' plugin, got '{base}'"
        ))),
        None => Err(DomainError::validation(format!(
            "{field}: '{plugin_ref}' is not a GTS plugin identifier"
        ))),
    }
}

fn validate_chain(chain: &PluginsConfig) -> DomainResult<()> {
    for binding in &chain.items {
        if gts_helpers::CATALOG_ONLY_PLUGIN_IDS.contains(&binding.plugin_ref.as_str()) {
            return Err(DomainError::validation(format!(
                "plugins.items: '{}' is a catalog-only identifier and cannot be bound",
                binding.plugin_ref
            )));
        }
        if binding.plugin_uuid().is_some() {
            continue;
        }
        let base = gts_helpers::base_part(&binding.plugin_ref).ok_or_else(|| {
            DomainError::validation(format!(
                "plugins.items: '{}' is not a GTS plugin identifier",
                binding.plugin_ref
            ))
        })?;
        if PluginKind::from_base_type(base).is_none() {
            return Err(DomainError::validation(format!(
                "plugins.items: '{}' is not a guard or transform plugin",
                binding.plugin_ref
            )));
        }
    }
    Ok(())
}

fn validate_rate_limit(config: &RateLimitConfig) -> DomainResult<()> {
    if config.sustained.rate == 0 {
        return Err(DomainError::validation(
            "rate_limit.sustained.rate must be at least 1",
        ));
    }
    if config.burst.capacity == Some(0) {
        return Err(DomainError::validation(
            "rate_limit.burst.capacity must be at least 1",
        ));
    }
    if config.cost == 0 {
        return Err(DomainError::validation(
            "rate_limit.cost must be at least 1",
        ));
    }
    if let Some(budget) = &config.budget
        && !(1.0..=2.0).contains(&budget.overcommit_ratio)
    {
        return Err(DomainError::validation(
            "rate_limit.budget.overcommit_ratio must be between 1.0 and 2.0",
        ));
    }
    Ok(())
}

/// Specificity of a route match, ordered lexicographically: the longest path
/// prefix wins, then the highest priority.
type RouteRank = (usize, i32);

/// Rank a candidate route.
fn route_rank(path_len: usize, priority: i32) -> RouteRank {
    (path_len, priority)
}

/// The best candidate so far: chain distance, rank, and the route itself.
type BestRoute<'a> = Option<(usize, RouteRank, &'a Route)>;

/// Whether `suffix` sits under the route's `path`.
fn path_matches(route_path: &str, suffix: &str) -> bool {
    let route_path = route_path.trim_end_matches('/');
    if route_path.is_empty() {
        return true;
    }
    if !suffix.starts_with(route_path) {
        return false;
    }
    // Only match on a segment boundary so `/v1/chat` does not swallow
    // `/v1/chatter`.
    matches!(suffix.as_bytes().get(route_path.len()), None | Some(b'/'))
}

#[async_trait]
impl ControlPlaneService for ControlPlaneServiceImpl {
    async fn create_upstream(
        &self,
        ctx: &SecurityContext,
        spec: UpstreamSpec,
    ) -> DomainResult<Upstream> {
        self.validate_upstream_spec(&spec)?;
        let tenant_id = ctx.subject_tenant_id();
        let resolved_alias =
            alias::resolve_create_alias(&spec.server.endpoints, spec.alias.as_deref())?;

        if self
            .upstreams
            .find_by_alias(tenant_id, &resolved_alias)
            .await?
            .is_some()
        {
            return Err(DomainError::conflict(format!(
                "an upstream with alias '{resolved_alias}' already exists for this tenant"
            )));
        }

        let plugins = spec.plugins.unwrap_or_default();
        self.bind_plugins(tenant_id, &plugins).await?;

        let upstream = Upstream {
            id: Uuid::new_v4(),
            tenant_id,
            alias: resolved_alias,
            enabled: spec.enabled.unwrap_or(true),
            tags: normalize_tags(spec.tags),
            server: normalize_server(spec.server),
            protocol: spec.protocol,
            auth: spec.auth,
            headers: spec.headers.unwrap_or_default(),
            plugins,
            rate_limit: spec.rate_limit,
            cors: spec.cors,
        };
        let created = self.upstreams.create(upstream).await?;
        self.sweep_plugins().await?;
        Ok(created)
    }

    async fn get_upstream(&self, ctx: &SecurityContext, id: Uuid) -> DomainResult<Upstream> {
        self.upstreams
            .get(ctx.subject_tenant_id(), id)
            .await?
            .ok_or_else(|| DomainError::not_found(format!("upstream '{id}' not found")))
    }

    async fn list_upstreams(&self, ctx: &SecurityContext) -> DomainResult<Vec<Upstream>> {
        self.upstreams.list(ctx.subject_tenant_id()).await
    }

    async fn replace_upstream(
        &self,
        ctx: &SecurityContext,
        id: Uuid,
        spec: UpstreamSpec,
    ) -> DomainResult<Upstream> {
        self.validate_upstream_spec(&spec)?;
        let tenant_id = ctx.subject_tenant_id();
        let existing = self
            .upstreams
            .get(tenant_id, id)
            .await?
            .ok_or_else(|| DomainError::not_found(format!("upstream '{id}' not found")))?;

        let resolved_alias = alias::enforce_alias_update(
            &existing.alias,
            &spec.server.endpoints,
            spec.alias.as_deref(),
        )?;

        let plugins = spec.plugins.unwrap_or_default();
        self.bind_plugins(tenant_id, &plugins).await?;

        let replaced = Upstream {
            id: existing.id,
            tenant_id: existing.tenant_id,
            alias: resolved_alias,
            enabled: spec.enabled.unwrap_or(true),
            tags: normalize_tags(spec.tags),
            server: normalize_server(spec.server),
            protocol: spec.protocol,
            auth: spec.auth,
            headers: spec.headers.unwrap_or_default(),
            plugins,
            rate_limit: spec.rate_limit,
            cors: spec.cors,
        };
        let saved = self.upstreams.replace(replaced).await?;
        self.sweep_plugins().await?;
        Ok(saved)
    }

    async fn delete_upstream(&self, ctx: &SecurityContext, id: Uuid) -> DomainResult<()> {
        let tenant_id = ctx.subject_tenant_id();
        if !self.upstreams.delete(tenant_id, id).await? {
            return Err(DomainError::not_found(format!("upstream '{id}' not found")));
        }
        self.routes.delete_by_upstream(id).await?;
        self.sweep_plugins().await?;
        Ok(())
    }

    async fn create_route(&self, ctx: &SecurityContext, spec: RouteSpec) -> DomainResult<Route> {
        let tenant_id = ctx.subject_tenant_id();
        let upstream_id = spec.upstream_id.ok_or_else(|| {
            DomainError::validation("upstream_id is required when creating a route")
        })?;
        // Ancestor upstreams are not directly addressable from the
        // management API, so this is scoped to the caller's tenant.
        let upstream = self
            .upstreams
            .get(tenant_id, upstream_id)
            .await?
            .ok_or_else(|| {
                DomainError::validation(format!(
                    "upstream '{upstream_id}' does not belong to this tenant"
                ))
            })?;

        self.validate_route_spec(&spec, &upstream)?;
        self.ensure_match_unique(upstream_id, &spec, None).await?;

        let plugins = spec.plugins.unwrap_or_default();
        self.bind_plugins(tenant_id, &plugins).await?;

        let route = Route {
            id: Uuid::new_v4(),
            tenant_id,
            upstream_id,
            enabled: spec.enabled.unwrap_or(true),
            priority: spec.priority.unwrap_or(0),
            tags: normalize_tags(spec.tags),
            match_config: spec.match_config,
            plugins,
            rate_limit: spec.rate_limit,
            cors: spec.cors,
        };
        let created = self.routes.create(route).await?;
        self.sweep_plugins().await?;
        Ok(created)
    }

    async fn get_route(&self, ctx: &SecurityContext, id: Uuid) -> DomainResult<Route> {
        self.routes
            .get(ctx.subject_tenant_id(), id)
            .await?
            .ok_or_else(|| DomainError::not_found(format!("route '{id}' not found")))
    }

    async fn list_routes(&self, ctx: &SecurityContext) -> DomainResult<Vec<Route>> {
        self.routes.list(ctx.subject_tenant_id()).await
    }

    async fn replace_route(
        &self,
        ctx: &SecurityContext,
        id: Uuid,
        spec: RouteSpec,
    ) -> DomainResult<Route> {
        let tenant_id = ctx.subject_tenant_id();
        let existing = self
            .routes
            .get(tenant_id, id)
            .await?
            .ok_or_else(|| DomainError::not_found(format!("route '{id}' not found")))?;
        let upstream = self
            .upstreams
            .get(tenant_id, existing.upstream_id)
            .await?
            .ok_or_else(|| {
                DomainError::not_found(format!("upstream '{}' not found", existing.upstream_id))
            })?;

        self.validate_route_spec(&spec, &upstream)?;
        self.ensure_match_unique(existing.upstream_id, &spec, Some(id))
            .await?;

        let plugins = spec.plugins.unwrap_or_default();
        self.bind_plugins(tenant_id, &plugins).await?;

        let replaced = Route {
            id: existing.id,
            tenant_id: existing.tenant_id,
            // `upstream_id` is immutable and absent from the update DTO.
            upstream_id: existing.upstream_id,
            enabled: spec.enabled.unwrap_or(true),
            priority: spec.priority.unwrap_or(0),
            tags: normalize_tags(spec.tags),
            match_config: spec.match_config,
            plugins,
            rate_limit: spec.rate_limit,
            cors: spec.cors,
        };
        let saved = self.routes.replace(replaced).await?;
        self.sweep_plugins().await?;
        Ok(saved)
    }

    async fn delete_route(&self, ctx: &SecurityContext, id: Uuid) -> DomainResult<()> {
        if !self.routes.delete(ctx.subject_tenant_id(), id).await? {
            return Err(DomainError::not_found(format!("route '{id}' not found")));
        }
        self.sweep_plugins().await?;
        Ok(())
    }

    async fn create_plugin(&self, ctx: &SecurityContext, spec: PluginSpec) -> DomainResult<Plugin> {
        if spec.name.trim().is_empty() {
            return Err(DomainError::validation("plugin name must not be empty"));
        }
        if spec.source_code.trim().is_empty() {
            return Err(DomainError::validation(
                "plugin source_code must not be empty",
            ));
        }
        if !spec.config_schema.is_object() && !spec.config_schema.is_null() {
            return Err(DomainError::validation(
                "plugin config_schema must be a JSON Schema object",
            ));
        }
        let now = Self::now_epoch_secs();
        let plugin = Plugin {
            id: Uuid::new_v4(),
            tenant_id: ctx.subject_tenant_id(),
            name: spec.name,
            description: spec.description,
            plugin_type: spec.plugin_type,
            phases: spec.phases,
            config_schema: spec.config_schema,
            source_code: spec.source_code,
            last_used_at: None,
            // Newly created definitions start unlinked; binding one clears
            // the marker.
            gc_eligible_at: Some(now + self.plugin_gc_ttl_secs),
        };
        self.plugins.create(plugin).await
    }

    async fn get_plugin(&self, ctx: &SecurityContext, id: Uuid) -> DomainResult<Plugin> {
        self.plugins
            .get(ctx.subject_tenant_id(), id)
            .await?
            .ok_or_else(|| DomainError::not_found(format!("plugin '{id}' not found")))
    }

    async fn list_plugins(&self, ctx: &SecurityContext) -> DomainResult<Vec<Plugin>> {
        self.plugins.list(ctx.subject_tenant_id()).await
    }

    async fn delete_plugin(&self, ctx: &SecurityContext, id: Uuid) -> DomainResult<()> {
        let tenant_id = ctx.subject_tenant_id();
        let plugin = self
            .plugins
            .get(tenant_id, id)
            .await?
            .ok_or_else(|| DomainError::not_found(format!("plugin '{id}' not found")))?;

        let (upstreams, routes) = self.plugin_references(plugin.id).await?;
        if !upstreams.is_empty() || !routes.is_empty() {
            let mut err = DomainError::plugin_in_use(format!(
                "Plugin is referenced by {} upstream(s) and {} route(s)",
                upstreams.len(),
                routes.len()
            ))
            .with_extension("plugin_id", plugin.gts_id());
            err = err.with_extension(
                "referenced_by",
                serde_json::json!({
                    "upstreams": upstreams
                        .iter()
                        .map(|id| gts_helpers::anonymous_id(gts_helpers::UPSTREAM_TYPE, *id))
                        .collect::<Vec<_>>(),
                    "routes": routes
                        .iter()
                        .map(|id| gts_helpers::anonymous_id(gts_helpers::ROUTE_TYPE, *id))
                        .collect::<Vec<_>>(),
                }),
            );
            return Err(err);
        }

        self.plugins.delete(tenant_id, id).await?;
        Ok(())
    }

    async fn resolve_proxy_target(
        &self,
        ctx: &SecurityContext,
        requested_alias: &str,
        method: &str,
        path_suffix: Option<&str>,
    ) -> DomainResult<ResolvedTarget> {
        let normalized = alias::normalize(requested_alias);
        let chain = resolution_chain(self.hierarchy.as_ref(), ctx, ctx.subject_tenant_id()).await?;

        // Descendant → root; the first hit is the routing target, but every
        // layer contributes to the effective configuration.
        let mut layers: Vec<Upstream> = Vec::new();
        for tenant in &chain {
            if let Some(upstream) = self.upstreams.find_by_alias(*tenant, &normalized).await? {
                layers.push(upstream);
            }
        }
        let Some(selected) = layers.first().cloned() else {
            return Err(DomainError::not_found(format!(
                "no upstream is registered for alias '{normalized}'"
            )));
        };

        // An ancestor disabling the upstream disables it for everyone below.
        let enabled_chain: Vec<bool> = layers.iter().rev().map(|u| u.enabled).collect();
        if !merge::effective_enabled(&enabled_chain) {
            return Err(DomainError::link_unavailable(format!(
                "upstream '{normalized}' is disabled"
            )));
        }

        if !selected.is_http() {
            return Err(DomainError::not_implemented(
                "gRPC proxying is not implemented in this build",
            ));
        }

        // Root-first ordering is what the merge helpers expect.
        let root_first: Vec<&Upstream> = layers.iter().rev().collect();

        let auth_layers: Vec<&AuthConfig> = root_first
            .iter()
            .map(|u| u.auth.as_ref().unwrap_or(EMPTY_AUTH.get_or_init(AuthConfig::default)))
            .collect();
        let auth = merge::merge_auth(&auth_layers).filter(|a| a.plugin_ref.is_some());

        let plugin_layers: Vec<&PluginsConfig> = root_first.iter().map(|u| &u.plugins).collect();
        let mut plugins = merge::merge_plugins(&plugin_layers);

        let tag_layers: Vec<&Vec<String>> = root_first.iter().map(|u| &u.tags).collect();
        let tags = merge::merge_tags(&tag_layers);

        let cors_layers: Vec<&CorsConfig> = root_first
            .iter()
            .map(|u| u.cors.as_ref().unwrap_or(EMPTY_CORS.get_or_init(CorsConfig::default)))
            .collect();
        let mut effective_cors = merge::merge_cors(&cors_layers).filter(|c| c.enabled);

        let upstream_limits: Vec<&RateLimitConfig> = root_first
            .iter()
            .filter_map(|u| u.rate_limit.as_ref())
            .collect();
        let upstream_rate_limit = merge::merge_rate_limit(&upstream_limits);

        // Routes are inherited along the chain; a descendant's route wins.
        let mut candidates: Vec<(usize, Route)> = Vec::new();
        for (distance, upstream) in layers.iter().enumerate() {
            for route in self.routes.list_by_upstream(upstream.id).await? {
                candidates.push((distance, route));
            }
        }

        let suffix = normalize_suffix(path_suffix);
        let matched = select_route(&candidates, method, suffix.as_deref());

        let mut rate_limits = Vec::new();
        if let Some(config) = upstream_rate_limit {
            rate_limits.push(RateLimitTarget {
                resource_type: "upstream",
                resource_id: selected.id,
                config,
            });
        }

        let mut headers = selected.headers.clone();
        // Header rules are not sharing-mode governed; an inherited upstream
        // supplies them when the selected layer declares none.
        if headers == HeadersConfig::default()
            && let Some(inherited) = root_first
                .iter()
                .rev()
                .find(|u| u.headers != HeadersConfig::default())
        {
            headers = inherited.headers.clone();
        }

        if let Some(route) = &matched {
            // Upstream bindings always run before route bindings. This is a
            // plain concatenation, *not* the hierarchy merge: sharing modes
            // govern what a descendant tenant inherits, and say nothing about
            // how an upstream's own chain composes with its route's.
            plugins = append_chain(plugins, &route.plugins);
            // Upstream < Route < Tenant (`cpt-cf-oagw-fr-config-layering`):
            // a route that declares CORS replaces the upstream's policy
            // outright. Sharing modes are a hierarchy concept and have
            // already been applied to `effective_cors` above.
            if let Some(route_cors) = &route.cors {
                effective_cors = Some(route_cors.clone()).filter(|c| c.enabled);
            }
            if let Some(route_limit) = &route.rate_limit {
                // A route may only tighten what enforcing ancestors allow.
                let enforced: Vec<&RateLimitConfig> = root_first
                    .iter()
                    .filter_map(|u| u.rate_limit.as_ref())
                    .filter(|rl| rl.sharing.is_enforced())
                    .collect();
                let mut layers_for_route = enforced;
                layers_for_route.push(route_limit);
                if let Some(config) = merge::merge_rate_limit(&layers_for_route) {
                    rate_limits.push(RateLimitTarget {
                        resource_type: "route",
                        resource_id: route.id,
                        config,
                    });
                }
            }
        }

        Ok(ResolvedTarget {
            upstream: selected,
            route: matched,
            auth,
            plugins,
            headers,
            cors: effective_cors,
            rate_limits,
            tags,
        })
    }

    async fn resolve_custom_plugin(&self, id: Uuid) -> DomainResult<Option<Plugin>> {
        self.plugins.get_any(id).await
    }
}

/// Shared empty layers so the merge helpers can take `&AuthConfig` /
/// `&CorsConfig` for upstreams that declare neither.
static EMPTY_AUTH: std::sync::OnceLock<AuthConfig> = std::sync::OnceLock::new();
static EMPTY_CORS: std::sync::OnceLock<CorsConfig> = std::sync::OnceLock::new();

fn normalize_tags(mut tags: Vec<String>) -> Vec<String> {
    tags.iter_mut().for_each(|t| *t = t.to_ascii_lowercase());
    tags.sort();
    tags.dedup();
    tags
}

fn normalize_server(mut server: ServerConfig) -> ServerConfig {
    for endpoint in &mut server.endpoints {
        endpoint.host = alias::normalize_host(&endpoint.host);
        endpoint.port = Some(endpoint.effective_port());
    }
    server
}

/// Append a route's chain after an upstream's, dropping duplicate bindings so
/// a plugin named at both levels still runs exactly once.
fn append_chain(mut upstream_chain: PluginsConfig, route_chain: &PluginsConfig) -> PluginsConfig {
    for binding in &route_chain.items {
        if !upstream_chain
            .items
            .iter()
            .any(|existing| existing.plugin_ref == binding.plugin_ref)
        {
            upstream_chain.items.push(binding.clone());
        }
    }
    upstream_chain
}

/// Trim the proxy-URL suffix to a leading-slash path, or `None` when absent.
fn normalize_suffix(suffix: Option<&str>) -> Option<String> {
    let suffix = suffix?.trim_matches('/');
    if suffix.is_empty() {
        None
    } else {
        Some(format!("/{suffix}"))
    }
}

/// Pick the winning route: longest path prefix, then highest priority, then
/// the closest tenant in the chain.
fn select_route(
    candidates: &[(usize, Route)],
    method: &str,
    suffix: Option<&str>,
) -> Option<Route> {
    let target = suffix.unwrap_or("/");
    let mut best: BestRoute<'_> = None;

    for (distance, route) in candidates {
        if !route.enabled {
            continue;
        }
        let Some(http) = &route.match_config.http else {
            continue;
        };
        if !http.methods.iter().any(|m| m.eq_ignore_ascii_case(method)) {
            continue;
        }
        if !path_matches(&http.path, target) {
            continue;
        }
        let rank = route_rank(http.path.trim_end_matches('/').len(), route.priority);
        let better = match &best {
            None => true,
            Some((best_distance, best_rank, _)) => {
                rank > *best_rank || (rank == *best_rank && distance < best_distance)
            }
        };
        if better {
            best = Some((*distance, rank, route));
        }
    }
    best.map(|(_, _, route)| route.clone())
}

/// Compute the outbound path for a matched route and inbound suffix.
///
/// # Errors
///
/// `400` when the route disables suffixes but the request carries one.
pub fn outbound_path(route: &Route, suffix: Option<&str>) -> DomainResult<String> {
    let Some(http) = &route.match_config.http else {
        return Err(DomainError::validation(
            "matched route has no HTTP match rules",
        ));
    };
    let base = if http.path.is_empty() {
        "/".to_owned()
    } else {
        http.path.clone()
    };
    let suffix = normalize_suffix(suffix);
    match (http.path_suffix_mode, suffix) {
        (PathSuffixMode::Disabled, Some(extra)) => {
            // A suffix equal to the route path is not "extra" — it is how the
            // caller addressed the route in the first place.
            if extra.trim_end_matches('/') == base.trim_end_matches('/') {
                Ok(base)
            } else {
                Err(DomainError::validation(format!(
                    "route '{base}' does not accept a path suffix (path_suffix_mode: disabled)"
                )))
            }
        }
        (PathSuffixMode::Disabled | PathSuffixMode::Append, None) => Ok(base),
        (PathSuffixMode::Append, Some(extra)) => {
            // The inbound suffix already contains the route path prefix.
            let trimmed_base = base.trim_end_matches('/');
            let remainder = extra.strip_prefix(trimmed_base).unwrap_or(extra.as_str());
            if remainder.is_empty() {
                Ok(base)
            } else {
                Ok(format!("{trimmed_base}{remainder}"))
            }
        }
    }
}

#[cfg(test)]
#[path = "management_tests.rs"]
mod tests;
