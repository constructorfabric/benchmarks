//! Control-plane services: validation and orchestration on top of the repos.
//!
//! Services are the only place that enforces the invariants of `DESIGN.md`
//! (alias uniqueness, route collision rules, referential integrity, endpoint
//! sanity). The REST layer stays a thin transport shell.

use std::sync::Arc;

use uuid::Uuid;

use crate::config::OagwConfig;
use crate::domain::dto::{
    self, CorsConfig, Endpoint, EndpointScheme, GrpcMatch, HttpMatch, MatchRules, Plugin,
    PluginBindings, PluginRef, RateLimitConfig, Route, RouteConfig, Upstream,
    UpstreamConfig, derive_alias, is_catalog_only_plugin, is_ip_literal, is_valid_hostname,
    parse_plugin_ref,
};
use crate::domain::error::DomainError;
use crate::domain::hierarchy::{TenantChain, TenantNode, effective_upstream, shares_with_descendants};
use crate::domain::plugin::PluginRegistry;
use crate::domain::repo::{ListQuery, Page, PluginRepo, RouteRepo, UpstreamRepo};

/// HTTP methods that are never accepted by a route.
const FORBIDDEN_METHODS: [&str; 2] = ["CONNECT", "TRACE"];

/// An upstream resolved across the caller's tenant chain.
#[derive(Debug, Clone)]
pub struct ResolvedUpstream {
    /// The effective configuration: the closest chain match with the graded
    /// configuration of the ancestors above it folded in.
    pub upstream: Upstream,
    /// Tenant ids of the caller's chain, descendant first. Route matching
    /// continues along it, so an inherited upstream keeps its ancestor's
    /// routes.
    pub chain: Vec<Uuid>,
}

/// Upstream lifecycle service.
pub struct UpstreamService {
    upstreams: Arc<dyn UpstreamRepo>,
    config: Arc<OagwConfig>,
    plugins: Arc<PluginRegistry>,
    chain: Arc<dyn TenantChain>,
}

impl std::fmt::Debug for UpstreamService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UpstreamService").finish()
    }
}

impl UpstreamService {
    /// Build the service.
    #[must_use]
    pub fn new(
        upstreams: Arc<dyn UpstreamRepo>,
        config: Arc<OagwConfig>,
        plugins: Arc<PluginRegistry>,
        chain: Arc<dyn TenantChain>,
    ) -> Self {
        Self {
            upstreams,
            config,
            plugins,
            chain,
        }
    }

    /// Create an upstream.
    ///
    /// # Errors
    /// See [`Self::validate`].
    pub async fn create(&self, tenant_id: Uuid, config: UpstreamConfig) -> Result<Upstream, DomainError> {
        self.validate(&config)?;
        let alias = config.resolve_alias().map_err(DomainError::InvalidConfiguration)?;
        // Hostname-based endpoints always auto-derive the alias; an explicit
        // alias that differs from the derived value is a validation error, and
        // only the exact derived value is tolerated (for idempotency).
        // DESIGN §"Alias Enforcement Rules".
        if let Some(requested) = config
            .alias
            .as_deref()
            .map(UpstreamConfig::normalize_alias)
            .filter(|alias| !alias.is_empty())
            && derive_alias(&config.server.endpoints).is_some()
        {
            let derived = derive_alias(&config.server.endpoints).unwrap_or_default();
            if requested != derived {
                return Err(DomainError::InvalidConfiguration(format!(
                    "alias '{requested}' differs from the alias derived from the endpoints ('{derived}'); hostname-based endpoints always auto-derive the alias"
                )));
            }
        }
        if self.upstreams.get_by_alias(tenant_id, &alias).await.is_ok() {
            return Err(DomainError::AliasConflict(alias));
        }
        let upstream = Upstream {
            id: Uuid::now_v7(),
            tenant_id,
            created_at: now_epoch(),
            // The resolved alias lives inside the flattened `config`, which is
            // where the wire schema keeps it.
            config: UpstreamConfig {
                alias: Some(alias),
                ..config
            },
        };
        self.upstreams.create(upstream).await
    }

    /// Replace an upstream's configuration.
    ///
    /// # Errors
    /// See [`Self::validate`]; `404` when the upstream does not exist.
    pub async fn update(
        &self,
        tenant_id: Uuid,
        id: Uuid,
        config: UpstreamConfig,
    ) -> Result<Upstream, DomainError> {
        self.validate(&config)?;
        let existing = self.upstreams.get(tenant_id, id).await?;
        let config = self.enforce_alias_update(&existing, config)?;
        self.upstreams.update(tenant_id, id, config).await
    }

    /// The alias is the routing key in `/proxy/{alias}/...`, so it is
    /// **immutable once set** (DESIGN §"Alias Update Behavior").
    ///
    /// * a payload that names a different alias is rejected — the operator has
    ///   to delete and re-create the upstream instead;
    /// * a payload that omits `alias` keeps the existing routing key, but an
    ///   endpoint change that would have altered the *derived* alias is still
    ///   rejected, so a PUT can never silently re-key an upstream;
    /// * an unchanged endpoint set keeps the alias untouched (the derived
    ///   value is only recomputed when the endpoints actually move).
    fn enforce_alias_update(
        &self,
        existing: &Upstream,
        mut config: UpstreamConfig,
    ) -> Result<UpstreamConfig, DomainError> {
        let Some(current) = existing.config.alias.clone() else {
            return Ok(config);
        };
        let endpoints_changed = config.server.endpoints != existing.config.server.endpoints;

        if let Some(requested) = config
            .alias
            .as_deref()
            .map(UpstreamConfig::normalize_alias)
            .filter(|alias| !alias.is_empty())
        {
            if requested != current {
                return Err(DomainError::InvalidConfiguration(format!(
                    "alias '{current}' is the routing key and cannot be re-keyed to '{requested}'; delete and re-create the upstream instead"
                )));
            }
            return Ok(config);
        }

        if endpoints_changed {
            match derive_alias(&config.server.endpoints) {
                Some(derived) if derived != current => {
                    return Err(DomainError::InvalidConfiguration(format!(
                        "changing the endpoints would re-key the upstream from '{current}' to '{derived}'; delete and re-create it instead"
                    )));
                }
                // Derivable → non-derivable: the derived alias would be lost.
                None if derive_alias(&existing.config.server.endpoints).is_some() => {
                    return Err(DomainError::InvalidConfiguration(format!(
                        "changing the endpoints would make the derived alias '{current}' underivable; delete and re-create the upstream instead"
                    )));
                }
                _ => {}
            }
        }
        config.alias = Some(current);
        Ok(config)
    }

    /// Fetch one upstream.
    ///
    /// # Errors
    /// `404` when missing or owned by another tenant.
    pub async fn get(&self, tenant_id: Uuid, id: Uuid) -> Result<Upstream, DomainError> {
        self.upstreams.get(tenant_id, id).await
    }

    /// Fetch one upstream by routing alias (data-plane entry point).
    ///
    /// # Errors
    /// `404` when the alias is unknown in this tenant.
    pub async fn resolve(&self, tenant_id: Uuid, alias: &str) -> Result<Upstream, DomainError> {
        let upstream = self.upstreams.get_by_alias(tenant_id, alias).await?;
        if !upstream.config.enabled {
            return Err(DomainError::LinkUnavailable(format!(
                "upstream '{alias}' is disabled"
            )));
        }
        Ok(upstream)
    }

    /// The caller's chain, descendant first.
    ///
    /// Without a security context there is nothing to resolve the hierarchy
    /// with, so the caller's own tenant is the whole chain.
    async fn chain_of(
        &self,
        ctx: Option<&crate::domain::SecurityContext>,
        tenant_id: Uuid,
    ) -> Vec<TenantNode> {
        match ctx {
            Some(security) => self.chain.chain(security, tenant_id).await,
            None => vec![TenantNode::own(tenant_id)],
        }
    }

    /// Resolve the upstream that serves `alias` for `tenant` across the tenant
    /// hierarchy (DESIGN §"Hierarchical Configuration", §"Shadowing Behavior").
    ///
    /// The walk goes descendant → root and stops at the first match: a
    /// definition closer to the caller shadows one above it. The graded
    /// configuration of the *other* chain members that define the same alias is
    /// folded into the result per field, so an `enforce`d ancestor constraint
    /// is never bypassed by shadowing and `effective_rate = min(selected,
    /// ancestors)`.
    ///
    /// A disabled definition is never stepped over: `503` is returned instead,
    /// which is what keeps "descendants MUST NOT re-enable an
    /// ancestor-disabled resource" true — an upstream an ancestor switched off
    /// stays unreachable however many shadows the tree holds.
    ///
    /// # Errors
    /// `404` when no member of the chain carries the alias, `503` when the
    /// effective upstream is disabled.
    pub async fn resolve_effective(
        &self,
        ctx: Option<&crate::domain::SecurityContext>,
        tenant_id: Uuid,
        alias: &str,
    ) -> Result<ResolvedUpstream, DomainError> {
        let nodes = self.chain_of(ctx, tenant_id).await;
        let mut selected: Option<Upstream> = None;
        let mut ancestors: Vec<Upstream> = Vec::new();
        for node in &nodes {
            if !node.active {
                continue;
            }
            // `get_by_alias` is case-insensitive: aliases are normalised to
            // lower case both here and when they are stored.
            let Ok(upstream) = self.upstreams.get_by_alias(node.id, alias).await else {
                continue;
            };
            match selected {
                // Closest match wins — no fall-through past a shadow.
                None => selected = Some(upstream),
                Some(_) => ancestors.push(upstream),
            }
        }

        let selected = selected.ok_or(DomainError::UpstreamNotFound)?;
        let refs = ancestors.iter().collect::<Vec<_>>();
        let upstream = effective_upstream(&selected, &refs);
        if !upstream.config.enabled {
            return Err(DomainError::LinkUnavailable(format!(
                "upstream '{alias}' is disabled"
            )));
        }
        Ok(ResolvedUpstream {
            upstream,
            chain: nodes
                .iter()
                .filter(|node| node.active)
                .map(|node| node.id)
                .collect(),
        })
    }

    /// Resolve an upstream for a CORS preflight.
    ///
    /// Browsers never send credentials on a preflight, so the edge middleware
    /// answers it with an anonymous security context: the tenant of the caller
    /// is unknown here. The lookup is tenant-scoped first and then falls back
    /// to an any-tenant scan; only the allow/deny decision is exposed, never a
    /// secret. A disabled upstream is not answered.
    ///
    /// # Errors
    /// `404` when no upstream carries `alias`, `503` when it is disabled.
    pub async fn resolve_for_preflight(&self, alias: &str) -> Result<Upstream, DomainError> {
        let candidates = self.upstreams.find_by_alias_any(alias).await?;
        let enabled = candidates
            .iter()
            .find(|upstream| upstream.config.enabled)
            .ok_or(DomainError::UpstreamNotFound)?;
        Ok(enabled.clone())
    }

    /// List upstreams.
    ///
    /// # Errors
    /// `400` on an unsupported `$filter`.
    pub async fn list(
        &self,
        tenant_id: Uuid,
        query: &ListQuery,
    ) -> Result<Page<Upstream>, DomainError> {
        self.upstreams.list(tenant_id, query).await
    }

    /// List a tenant's upstreams plus the definitions its ancestors share with
    /// it (DESIGN §"Hierarchical Configuration" visibility).
    ///
    /// An ancestor definition appears only when it actually carries a shared
    /// field (`inherit` or `enforce`): a `private` one stays invisible, which
    /// is the default. Shared definitions are read-only for the descendant —
    /// `get` / `put` / `delete` by id remain strictly tenant-scoped — so the
    /// listing is how a descendant discovers what it inherits.
    ///
    /// # Errors
    /// `400` on an unsupported `$filter`.
    pub async fn list_visible(
        &self,
        ctx: Option<&crate::domain::SecurityContext>,
        tenant_id: Uuid,
        query: &ListQuery,
    ) -> Result<Page<Upstream>, DomainError> {
        let own = self.upstreams.list(tenant_id, query).await?;
        let mut inherited: Vec<Upstream> = Vec::new();
        for node in self.chain_of(ctx, tenant_id).await {
            if !node.active || node.id == tenant_id {
                continue;
            }
            let page = self.upstreams.list(node.id, query).await?;
            inherited.extend(
                page.items
                    .into_iter()
                    .filter(|upstream| shares_with_descendants(&upstream.config)),
            );
        }
        if inherited.is_empty() {
            return Ok(own);
        }

        // The merge has to be paginated with the same OData semantics the
        // repository applies to a single-tenant page (filter, search, order,
        // top, skip), so the combined vector goes through the same helper —
        // after the same default ordering the repository uses.
        let mut combined = own.items;
        combined.extend(inherited);
        combined.sort_by(|a, b| {
            a.config
                .alias
                .cmp(&b.config.alias)
                .then_with(|| a.id.cmp(&b.id))
        });
        crate::infra::storage::paginate(combined, query, |_| true)
    }

    /// Delete an upstream when no route references it.
    ///
    /// # Errors
    /// `404` when missing, `409` when routes still reference it.
    pub async fn delete(&self, tenant_id: Uuid, id: Uuid) -> Result<(), DomainError> {
        let refs = self.upstreams.route_count(tenant_id, id).await?;
        if refs > 0 {
            return Err(DomainError::UpstreamInUse(refs));
        }
        self.upstreams.delete(tenant_id, id).await
    }

    /// Validate an upstream payload against the DESIGN rules.
    ///
    /// # Errors
    /// [`DomainError::Validation`] / [`DomainError::InvalidConfiguration`] /
    /// [`DomainError::SchemeNotAllowed`].
    pub fn validate(&self, config: &UpstreamConfig) -> Result<(), DomainError> {
        if config.server.endpoints.is_empty() {
            return Err(DomainError::Validation(
                "server.endpoints must contain at least one endpoint".into(),
            ));
        }
        if config.server.endpoints.len() > 64 {
            return Err(DomainError::Validation(
                "server.endpoints must contain at most 64 endpoints".into(),
            ));
        }
        for endpoint in &config.server.endpoints {
            self.validate_endpoint(endpoint)?;
        }
        if let Some(alias) = config.alias.as_deref() {
            let normalized = UpstreamConfig::normalize_alias(alias);
            if !dto::is_valid_alias(&normalized) {
                return Err(DomainError::Validation(format!(
                    "alias '{normalized}' does not match ^[a-z0-9]([a-z0-9.:-]*[a-z0-9])?$"
                )));
            }
        }
        if config.tags.len() > 64 {
            return Err(DomainError::Validation("at most 64 tags are allowed".into()));
        }
        validate_plugin_bindings(&self.plugins, &config.plugins)?;
        if let Some(auth) = &config.auth {
            self.validate_auth(auth)?;
        }
        if let Some(rate_limit) = &config.rate_limit {
            validate_rate_limit(rate_limit)?;
        }
        if let Some(cors) = &config.cors {
            validate_cors(cors)?;
        }
        Ok(())
    }

    fn validate_endpoint(&self, endpoint: &Endpoint) -> Result<(), DomainError> {
        let host = endpoint.normalized_host();
        if host.is_empty() {
            return Err(DomainError::MissingTargetHost);
        }
        if host.len() > 253 {
            return Err(DomainError::InvalidTargetHost(host));
        }
        if !is_ip_literal(&host) && !is_valid_hostname(&host) {
            return Err(DomainError::InvalidTargetHost(host));
        }
        if endpoint.scheme == EndpointScheme::Http && !self.config.allow_http_upstream {
            return Err(DomainError::SchemeNotAllowed(
                "http upstreams are disabled by allow_http_upstream=false".into(),
            ));
        }
        if endpoint.port.is_some_and(|p| p == 0) {
            return Err(DomainError::Validation(format!(
                "endpoint port 0 is not valid for host '{host}'"
            )));
        }
        Ok(())
    }

    fn validate_auth(&self, auth: &dto::AuthConfig) -> Result<(), DomainError> {
        let Some(reference) = auth.plugin_type.as_deref().or(auth.plugin_ref.as_deref()) else {
            return Err(DomainError::Validation(
                "auth requires a 'type' naming the auth plugin".into(),
            ));
        };
        if is_catalog_only_plugin(reference) {
            return Err(DomainError::Validation(format!(
                "auth plugin '{reference}' is catalogued but not implemented; it may not be bound to a resource"
            )));
        }
        if let Some(plugin) = self.plugins.auth(reference) {
            return plugin.validate_config(&auth.config);
        }
        Ok(())
    }
}

/// Validate a plugin chain binding against the built-in registry.
///
/// Shared by the upstream and the route validators: a reserved, catalog-only id
/// is refused outright, an unknown built-in id stays configurable (a newer
/// catalog may be provisioned ahead of this build) and a custom reference is
/// resolved by the data plane.
///
/// # Errors
/// [`DomainError::Validation`] for a reserved catalog-only plugin.
pub(crate) fn validate_plugin_bindings(
    registry: &PluginRegistry,
    bindings: &PluginBindings,
) -> Result<(), DomainError> {
    if bindings.items.len() > 32 {
        return Err(DomainError::Validation(
            "at most 32 plugins may be bound to one resource".into(),
        ));
    }
    for reference in &bindings.items {
        match parse_plugin_ref(reference) {
            PluginRef::Custom(_) => {}
            PluginRef::Named(id) => {
                if is_catalog_only_plugin(&id) {
                    return Err(DomainError::Validation(format!(
                        "plugin '{id}' is catalogued but not implemented; it may not be bound to a resource"
                    )));
                }
                let is_auth = registry.auth(&id).is_some();
                let is_guard = registry.guard(&id).is_some();
                let is_transform = registry.transform(&id).is_some();
                if !is_auth && !is_guard && !is_transform {
                    tracing::debug!(plugin = %id, "unknown built-in plugin reference");
                }
            }
        }
    }
    Ok(())
}

/// Route lifecycle service.
pub struct RouteService {
    routes: Arc<dyn RouteRepo>,
    upstreams: Arc<dyn UpstreamRepo>,
    plugins: Arc<PluginRegistry>,
}

impl std::fmt::Debug for RouteService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RouteService").finish()
    }
}

impl RouteService {
    /// Build the service.
    #[must_use]
    pub fn new(
        routes: Arc<dyn RouteRepo>,
        upstreams: Arc<dyn UpstreamRepo>,
        plugins: Arc<PluginRegistry>,
    ) -> Self {
        Self {
            routes,
            upstreams,
            plugins,
        }
    }

    /// Create a route.
    ///
    /// # Errors
    /// See [`Self::validate`].
    pub async fn create(&self, tenant_id: Uuid, config: RouteConfig) -> Result<Route, DomainError> {
        self.validate(tenant_id, &config).await?;
        let route = Route {
            id: Uuid::now_v7(),
            tenant_id,
            upstream_id: config.upstream_id,
            created_at: now_epoch(),
            config,
        };
        self.routes.create(route).await
    }

    /// Replace a route's configuration.
    ///
    /// # Errors
    /// See [`Self::validate`]; `404` when the route does not exist.
    pub async fn update(
        &self,
        tenant_id: Uuid,
        id: Uuid,
        config: RouteConfig,
    ) -> Result<Route, DomainError> {
        let existing = self.routes.get(tenant_id, id).await?;
        // DESIGN §"Immutable fields": `Route.upstream_id` is immutable after
        // create, so a PUT that re-targets the route is refused instead of
        // silently moving its traffic to another upstream.
        if config.upstream_id != existing.upstream_id {
            return Err(DomainError::Validation(format!(
                "upstream_id is immutable: this route stays bound to '{}'",
                existing.upstream_id
            )));
        }
        self.validate(tenant_id, &config).await?;
        self.routes.update(tenant_id, id, config).await
    }

    /// Fetch one route.
    ///
    /// # Errors
    /// `404` when missing or owned by another tenant.
    pub async fn get(&self, tenant_id: Uuid, id: Uuid) -> Result<Route, DomainError> {
        self.routes.get(tenant_id, id).await
    }

    /// List routes of a tenant.
    ///
    /// # Errors
    /// Propagated from the repository.
    pub async fn list(&self, tenant_id: Uuid, query: &ListQuery) -> Result<Page<Route>, DomainError> {
        self.routes.list(tenant_id, query).await
    }

    /// List routes that target one upstream.
    ///
    /// # Errors
    /// Propagated from the repository.
    pub async fn list_by_upstream(
        &self,
        tenant_id: Uuid,
        upstream_id: Uuid,
    ) -> Result<Vec<Route>, DomainError> {
        self.routes.list_by_upstream(tenant_id, upstream_id).await
    }

    /// Delete a route.
    ///
    /// # Errors
    /// `404` when missing.
    pub async fn delete(&self, tenant_id: Uuid, id: Uuid) -> Result<(), DomainError> {
        self.routes.delete(tenant_id, id).await
    }

    /// Validate a route configuration.
    ///
    /// # Errors
    /// [`DomainError::Validation`], `404` for an unknown upstream, `409` on a
    /// match-rule collision.
    pub async fn validate(&self, tenant_id: Uuid, config: &RouteConfig) -> Result<(), DomainError> {
        self.upstreams.get(tenant_id, config.upstream_id).await?;
        validate_plugin_bindings(&self.plugins, &config.plugins)?;
        match &config.matcher {
            MatchRules::Http(matcher) => validate_http_match(matcher)?,
            MatchRules::Grpc(matcher) => validate_grpc_match(matcher)?,
        }
        if let Some(rate_limit) = &config.rate_limit {
            validate_rate_limit(rate_limit)?;
        }
        Ok(())
    }

    /// Resolve the winning route for `(method, path, query)`.
    ///
    /// # Errors
    /// Propagated from the repository.
    pub async fn resolve(
        &self,
        tenant_id: Uuid,
        upstream_id: Uuid,
        method: &str,
        path: &str,
        query: &[(String, String)],
    ) -> Result<Option<Route>, DomainError> {
        self.routes
            .find_match(tenant_id, upstream_id, method, path, query)
            .await
    }

    /// Resolve the winning route along a tenant chain.
    ///
    /// `chain` is ordered descendant first, as produced by
    /// [`UpstreamService::resolve_effective`]. The first tenant of the chain
    /// that owns a matching route wins, so a descendant's route shadows its
    /// ancestors' — an inherited upstream keeps the routes its owner wrote,
    /// unless a closer tenant overrides the same match space.
    ///
    /// # Errors
    /// Propagated from the repository.
    pub async fn resolve_in_chain(
        &self,
        chain: &[Uuid],
        upstream_id: Uuid,
        method: &str,
        path: &str,
        query: &[(String, String)],
    ) -> Result<Option<Route>, DomainError> {
        for tenant in chain {
            if let Some(route) = self
                .routes
                .find_match(*tenant, upstream_id, method, path, query)
                .await?
            {
                return Ok(Some(route));
            }
        }
        Ok(None)
    }
}

/// Tenant-defined plugin lifecycle service.
pub struct PluginService {
    plugins: Arc<dyn PluginRepo>,
}

impl std::fmt::Debug for PluginService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PluginService").finish()
    }
}

impl PluginService {
    /// Build the service.
    #[must_use]
    pub fn new(plugins: Arc<dyn PluginRepo>) -> Self {
        Self { plugins }
    }

    /// Create a tenant plugin.
    ///
    /// # Errors
    /// [`DomainError::Validation`] on an empty name or source.
    pub async fn create(&self, tenant_id: Uuid, plugin: Plugin) -> Result<Plugin, DomainError> {
        validate_plugin(&plugin)?;
        let stored = Plugin {
            id: Uuid::now_v7(),
            tenant_id,
            created_at: now_epoch(),
            ..plugin
        };
        self.plugins.create(stored).await
    }

    /// Fetch one plugin.
    ///
    /// # Errors
    /// `404` when missing or owned by another tenant.
    pub async fn get(&self, tenant_id: Uuid, id: Uuid) -> Result<Plugin, DomainError> {
        self.plugins.get(tenant_id, id).await
    }

    /// List plugins of a tenant.
    ///
    /// # Errors
    /// Propagated from the repository.
    pub async fn list(&self, tenant_id: Uuid, query: &ListQuery)
        -> Result<Page<Plugin>, DomainError>
    {
        self.plugins.list(tenant_id, query).await
    }

    /// Delete a plugin when nothing references it.
    ///
    /// # Errors
    /// `404` when missing, `409` when referenced.
    pub async fn delete(&self, tenant_id: Uuid, id: Uuid) -> Result<(), DomainError> {
        let refs = self.plugins.reference_count(tenant_id, id).await?;
        if refs > 0 {
            return Err(DomainError::PluginInUse);
        }
        self.plugins.delete(tenant_id, id).await
    }
}

/// Validate an HTTP match rule.
///
/// # Errors
/// [`DomainError::Validation`].
pub fn validate_http_match(matcher: &HttpMatch) -> Result<(), DomainError> {
    if matcher.methods.is_empty() {
        return Err(DomainError::Validation(
            "match.methods must list at least one method".into(),
        ));
    }
    if matcher.methods.len() > 16 {
        return Err(DomainError::Validation(
            "match.methods must list at most 16 methods".into(),
        ));
    }
    for method in &matcher.methods {
        let upper = method.to_ascii_uppercase();
        if !upper.bytes().all(|b| b.is_ascii_alphabetic()) {
            return Err(DomainError::Validation(format!(
                "match.methods contains the invalid token '{method}'"
            )));
        }
        if FORBIDDEN_METHODS.contains(&upper.as_str()) {
            return Err(DomainError::Validation(format!(
                "method '{upper}' may not be routed through OAGW"
            )));
        }
    }
    if matcher.path.is_empty() || !matcher.path.starts_with('/') {
        return Err(DomainError::Validation(
            "match.path must be an absolute path starting with '/'".into(),
        ));
    }
    if matcher.path.contains('?') || matcher.path.contains('#') {
        return Err(DomainError::Validation(
            "match.path must not contain a query or fragment".into(),
        ));
    }
    if matcher.query_allowlist.len() > 64 {
        return Err(DomainError::Validation(
            "match.query_allowlist must contain at most 64 entries".into(),
        ));
    }
    Ok(())
}

/// Validate a gRPC match rule.
///
/// # Errors
/// [`DomainError::Validation`].
pub fn validate_grpc_match(matcher: &GrpcMatch) -> Result<(), DomainError> {
    if matcher.service.is_empty() {
        return Err(DomainError::Validation(
            "grpc match.service must name a fully-qualified service".into(),
        ));
    }
    Ok(())
}

/// Validate a rate limit configuration.
///
/// # Errors
/// [`DomainError::Validation`].
pub fn validate_rate_limit(config: &RateLimitConfig) -> Result<(), DomainError> {
    if config.sustained.rate == 0 {
        return Err(DomainError::Validation(
            "rate_limit.sustained.rate must be greater than zero".into(),
        ));
    }
    if config.burst.capacity == 0 {
        return Err(DomainError::Validation(
            "rate_limit.burst.capacity must be greater than zero".into(),
        ));
    }
    if config.cost == 0 {
        return Err(DomainError::Validation(
            "rate_limit.cost must be greater than zero".into(),
        ));
    }
    Ok(())
}

/// Validate a CORS configuration.
///
/// # Errors
/// [`DomainError::Validation`].
pub fn validate_cors(config: &CorsConfig) -> Result<(), DomainError> {
    if !config.enabled {
        return Ok(());
    }
    if config.allowed_origins.is_empty() {
        return Err(DomainError::Validation(
            "cors.allowed_origins must be non-empty when cors is enabled".into(),
        ));
    }
    if config.allowed_methods.is_empty() {
        return Err(DomainError::Validation(
            "cors.allowed_methods must be non-empty when cors is enabled".into(),
        ));
    }
    Ok(())
}

/// Validate a tenant-defined plugin.
///
/// # Errors
/// [`DomainError::Validation`].
pub fn validate_plugin(plugin: &Plugin) -> Result<(), DomainError> {
    if plugin.name.trim().is_empty() {
        return Err(DomainError::Validation("plugin.name is required".into()));
    }
    if plugin.name.len() > 128 {
        return Err(DomainError::Validation(
            "plugin.name must be at most 128 characters".into(),
        ));
    }
    if plugin.source.len() > 256 * 1024 {
        return Err(DomainError::Validation(
            "plugin.source must be at most 256 KiB".into(),
        ));
    }
    Ok(())
}

/// Current unix time in seconds.
#[must_use]
pub fn now_epoch() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn upstream_config(host: &str, scheme: EndpointScheme) -> UpstreamConfig {
        UpstreamConfig {
            server: crate::domain::dto::Server {
                endpoints: vec![Endpoint {
                    scheme,
                    host: host.to_owned(),
                    port: Some(8080),
                }],
            },
            ..UpstreamConfig::default()
        }
    }

    #[test]
    fn http_scheme_is_rejected_when_disabled() {
        let config = Arc::new(OagwConfig::default());
        let registry = Arc::new(PluginRegistry::new());
        let service = UpstreamService {
            upstreams: Arc::new(crate::infra::storage::InMemoryStore::default()),
            config,
            plugins: registry,
            chain: Arc::new(crate::domain::hierarchy::SingleTenantChain),
        };
        let err = service
            .validate(&upstream_config("api.example.com", EndpointScheme::Http))
            .expect_err("http must be rejected by default");
        assert_eq!(err.status(), 400);
    }

    #[test]
    fn https_upstreams_are_always_accepted() {
        let config = Arc::new(OagwConfig::default());
        let registry = Arc::new(PluginRegistry::new());
        let service = UpstreamService {
            upstreams: Arc::new(crate::infra::storage::InMemoryStore::default()),
            config,
            plugins: registry,
            chain: Arc::new(crate::domain::hierarchy::SingleTenantChain),
        };
        service
            .validate(&upstream_config("api.openai.com", EndpointScheme::Https))
            .expect("https is legal");
    }

    #[test]
    fn http_scheme_is_accepted_when_enabled() {
        let mut config = OagwConfig::default();
        config.allow_http_upstream = true;
        let registry = Arc::new(PluginRegistry::new());
        let service = UpstreamService {
            upstreams: Arc::new(crate::infra::storage::InMemoryStore::default()),
            config: Arc::new(config),
            plugins: registry,
            chain: Arc::new(crate::domain::hierarchy::SingleTenantChain),
        };
        service
            .validate(&upstream_config("localhost", EndpointScheme::Http))
            .expect("http is legal when allow_http_upstream");
    }

    #[test]
    fn empty_endpoint_list_is_a_validation_error() {
        let registry = Arc::new(PluginRegistry::new());
        let service = UpstreamService {
            upstreams: Arc::new(crate::infra::storage::InMemoryStore::default()),
            config: Arc::new(OagwConfig::default()),
            plugins: registry,
            chain: Arc::new(crate::domain::hierarchy::SingleTenantChain),
        };
        assert!(service.validate(&UpstreamConfig::default()).is_err());
    }

    #[test]
    fn match_validation_rejects_bad_paths_and_methods() {
        let mut matcher = HttpMatch::default();
        matcher.methods = vec![String::from("GET")];
        matcher.path = String::from("nope");
        assert!(validate_http_match(&matcher).is_err());
        matcher.path = String::from("/v1");
        assert!(validate_http_match(&matcher).is_ok());
        matcher.methods = vec![String::from("CONNECT")];
        assert!(validate_http_match(&matcher).is_err());
    }
}
