//! Persistence-free authoritative control-plane repository.
//!
//! ## Persistence-free MVP (mandated deviation from the DB-capable design)
//!
//! The FEATURE docs describe a `db` capability and `ctx.db_required()`-backed
//! storage. The implementation mandate for this run is persistence-free per
//! ADR 0010's persistence-free MVP clause: the gear declares **no `db`
//! capability** (and no DB migration crate in its manifest), so the
//! authoritative control plane is implemented here as a thread-safe,
//! in-memory repository (`DashMap` + `parking_lot`) modelling the `oagw_*`
//! tables of DESIGN.md §3.7. The repository is config-seeded and fully
//! CRUD-able with authoritative management semantics; nothing else in the
//! gear assumes a database. If a future milestone adds persistence, the
//! repository trait/impl boundary keeps the swap contained.
//!
//! `ControlPlaneService` is also the owner of effective-config invalidation:
//! every mutation bumps the L1 config-cache generation so the data plane
//! converges lazily but promptly.

use std::sync::Arc;
use std::time::Duration;

use dashmap::DashMap;
use parking_lot::RwLock;
use toolkit_security::constants::DEFAULT_TENANT_ID;
use tracing::{debug, info};

use crate::config::OagwConfig;
use crate::domain::cors::CorsGate;
use crate::domain::effective::L1ConfigCache;
use crate::domain::error::DomainError;
use crate::domain::models::{
    CorsConfig, Plugin, PluginKind, Route, Upstream, UpstreamScheme, default_port_for,
};

/// Alias validation rule: lower-case alnum start, then `[a-z0-9._-]`,
/// no more than 255 chars (`inst-alias-rules`).
fn validate_alias(alias: &str) -> Result<(), DomainError> {
    let mut chars = alias.chars();
    let first_ok = chars
        .next()
        .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit());
    if !first_ok
        || !chars
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | '-'))
        || alias.len() > 255
        || alias.is_empty()
    {
        return Err(DomainError::validation(
            "alias must be 1-255 chars of [a-z0-9] plus [._-], starting alphanumeric",
        ));
    }
    Ok(())
}

/// Thin alias used at the REST boundary; kept separate so repo methods can
/// return `Result`s without leaking implementation details.
pub type RepositoryError = DomainError;

/// A delete-prune hook fired with the deleted alias so downstream maps
/// (rate-limit buckets, load balancers) can release per-entity state.
type DeleteHook = Box<dyn Fn(&str) + Send + Sync>;

/// Fan-out registry of [`DeleteHook`]s guarded by a read-write lock.
type DeleteHooks = Arc<RwLock<Vec<DeleteHook>>>;

/// In-memory authoritative control-plane repository (see module docs).
pub struct ControlPlaneService {
    /// `oagw_upstream` — keyed by alias.
    upstreams: DashMap<String, Upstream>,
    /// `oagw_route` — keyed by alias.
    routes: DashMap<String, Route>,
    /// `oagw_plugin` — keyed by alias.
    plugins: DashMap<String, Plugin>,
    /// `oagw_upstream_plugin` — upstream alias -> plugin alias list.
    upstream_bindings: DashMap<String, Arc<RwLock<Vec<String>>>>,
    /// `oagw_route_plugin` — route alias -> plugin alias list.
    route_bindings: DashMap<String, Arc<RwLock<Vec<String>>>>,
    /// L1 effective-config cache invalidated by every mutation.
    l1_cache: L1ConfigCache,
    /// Default proxy timeout for routes without their own override.
    default_timeout: Duration,
    /// Whether `http://` upstreams are permitted.
    allow_http_upstream: bool,
    /// The tenant this gear serves (single-tenant MVP scope gate).
    serving_tenant: String,
    /// Prune hooks fired after a route is deleted so downstream maps
    /// (rate-limit buckets) release per-route state.
    route_delete_hooks: DeleteHooks,
    /// Prune hooks fired after an upstream is deleted so downstream maps
    /// (load balancers) release per-upstream state.
    upstream_delete_hooks: DeleteHooks,
}

impl ControlPlaneService {
    /// Creates an empty repository with the given data-plane defaults.
    #[must_use]
    pub fn new(
        token_cache_capacity_hint: usize,
        default_timeout: Duration,
        allow_http_upstream: bool,
    ) -> Self {
        // The token-cache hint is accepted for symmetry with the config that
        // drives the OAuth token cache sizing (kept in `infra/proxy`); the
        // control-plane store itself has no fixed capacity.
        let _ = token_cache_capacity_hint;
        debug!(
            ?default_timeout,
            allow_http_upstream, "initialized in-memory control-plane repository"
        );
        Self {
            upstreams: DashMap::new(),
            routes: DashMap::new(),
            plugins: DashMap::new(),
            upstream_bindings: DashMap::new(),
            route_bindings: DashMap::new(),
            l1_cache: L1ConfigCache::new(),
            default_timeout,
            allow_http_upstream,
            serving_tenant: "root".to_owned(),
            route_delete_hooks: Arc::new(RwLock::new(Vec::new())),
            upstream_delete_hooks: Arc::new(RwLock::new(Vec::new())),
        }
    }

    /// Builds and seeds the repository from validated gear config.
    ///
    /// # Errors
    ///
    /// Returns an error when the config is invalid or seeding conflicts.
    pub fn from_config(cfg: &OagwConfig) -> anyhow::Result<Self> {
        cfg.validate()?;
        let repo = Self::new(
            cfg.token_cache_capacity,
            Duration::from_secs(cfg.proxy_timeout_secs),
            cfg.allow_http_upstream,
        );

        for seed in &cfg.upstreams {
            let scheme = UpstreamScheme::parse(&seed.scheme).ok_or_else(|| {
                anyhow::anyhow!("oagw: unknown upstream scheme `{}`", seed.scheme)
            })?;
            let upstream = Upstream {
                alias: seed.alias.clone(),
                name: if seed.name.is_empty() {
                    seed.alias.clone()
                } else {
                    seed.name.clone()
                },
                host: seed.host.clone(),
                port: seed.port.unwrap_or_else(|| default_port_for(scheme)),
                scheme,
                path_prefix: seed.path_prefix.clone(),
                enabled: seed.enabled,
                timeout_secs: 0,
            };
            repo.upsert_upstream(upstream)?;
        }

        for seed in &cfg.routes {
            let route = Route {
                alias: seed.alias.clone(),
                upstream_alias: Some(seed.upstream_alias.clone()),
                methods: seed.methods.clone(),
                http_matches: Vec::new(),
                rate_limit: seed.rate_limit.clone().map(|r| {
                    crate::domain::models::RateLimitConfig {
                        capacity: r.capacity,
                        refill_per_sec: r.refill_per_sec,
                    }
                }),
                cors: seed
                    .cors
                    .clone()
                    .map_or_else(CorsConfig::default, |c| CorsConfig {
                        enabled: c.enabled,
                        allowed_origins: c.allowed_origins,
                        allowed_methods: c.allowed_methods,
                        allowed_headers: c.allowed_headers,
                        expose_headers: c.expose_headers,
                        max_age_secs: c.max_age_secs,
                        allow_credentials: c.allow_credentials,
                    }),
                enabled: seed.enabled,
                priority: seed.rate_limit.as_ref().map_or(0, |_| 0),
            };
            repo.upsert_route(route)?;
        }

        for seed in &cfg.plugins {
            let kind = PluginKind::parse(&seed.kind)
                .ok_or_else(|| anyhow::anyhow!("oagw: unknown plugin kind `{}`", seed.kind))?;
            let plugin = Plugin {
                alias: seed.alias.clone(),
                kind,
                enabled: seed.enabled,
                config: seed.config.clone(),
            };
            repo.upsert_plugin(plugin)?;
            for ua in &seed.upstreams {
                repo.bind_plugin_to_upstream(ua, &seed.alias)?;
            }
            for ra in &seed.routes {
                repo.bind_plugin_to_route(ra, &seed.alias)?;
            }
        }

        let upstream_count = repo.upstreams.len();
        let route_count = repo.routes.len();
        let plugin_count = repo.plugins.len();
        info!(
            upstream_count,
            route_count, plugin_count, "seeded in-memory control-plane repository from config"
        );
        Ok(repo)
    }

    /// Accessor for the L1 cache (used by the data plane).
    #[must_use]
    pub fn l1_cache(&self) -> &L1ConfigCache {
        &self.l1_cache
    }

    /// Enforces the tenant-scope gate for management operations
    /// (`inst-tenant-scope` / `inst-route-tenant`).
    ///
    /// # Errors
    ///
    /// Returns `TenantScope` when the subject tenant is not the serving tenant.
    pub fn assert_tenant(&self, subject_tenant: &str, resource: &str) -> Result<(), DomainError> {
        // The serving tenant is the platform's system/root tenant. Callers
        // authenticated to either the gear's own serving-tenant name (`root`)
        // or the platform's default system tenant UUID (`DEFAULT_TENANT_ID`,
        // used by static-authn / single-tenant deployments) are in scope;
        // every other tenant is denied (`inst-tenant-scope`).
        let system_root = DEFAULT_TENANT_ID.to_string();
        if self.serving_tenant.is_empty()
            || subject_tenant == self.serving_tenant
            || subject_tenant == system_root
        {
            Ok(())
        } else {
            Err(DomainError::TenantScope(resource.to_owned()))
        }
    }

    // ------------------------------------------------------------------
    // Upstreams
    // ------------------------------------------------------------------

    /// Lists every upstream.
    #[must_use]
    pub fn list_upstreams(&self) -> Vec<Upstream> {
        let mut v: Vec<Upstream> = self.upstreams.iter().map(|e| e.value().clone()).collect();
        v.sort_by(|a, b| a.alias.cmp(&b.alias));
        v
    }

    /// Looks up an upstream by alias.
    #[must_use]
    pub fn get_upstream(&self, alias: &str) -> Option<Upstream> {
        self.upstreams.get(alias).map(|e| e.value().clone())
    }

    /// Creates or fully replaces an upstream (`oagw_upstream`).
    ///
    /// # Errors
    ///
    /// Returns `Validation` / `TransportNotPermitted` / `UpstreamAliasConflict`
    /// when the payload is invalid or the alias is already bound.
    pub fn upsert_upstream(&self, upstream: Upstream) -> Result<(), DomainError> {
        validate_alias(&upstream.alias)?;
        if upstream.host.trim().is_empty() {
            return Err(DomainError::validation("upstream `host` must not be empty"));
        }
        match upstream.scheme {
            UpstreamScheme::Https => {}
            UpstreamScheme::Http if self.allow_http_upstream => {}
            UpstreamScheme::Http => {
                return Err(DomainError::TransportNotPermitted(
                    upstream.alias.clone(),
                    "plaintext http upstreams are disabled (allow_http_upstream=false)".to_owned(),
                ));
            }
        }
        if upstream.port == 0 {
            return Err(DomainError::validation("upstream `port` must be > 0"));
        }
        if self.upstreams.contains_key(&upstream.alias) {
            return Err(DomainError::UpstreamAliasConflict(upstream.alias));
        }
        self.upstreams.insert(upstream.alias.clone(), upstream);
        self.invalidate_all();
        Ok(())
    }

    /// Updates an existing upstream in place.
    ///
    /// # Errors
    ///
    /// Returns `UpstreamNotFound` when the alias is unknown.
    pub fn update_upstream(&self, upstream: Upstream) -> Result<(), DomainError> {
        if !self.upstreams.contains_key(&upstream.alias) {
            return Err(DomainError::UpstreamNotFound(upstream.alias));
        }
        // Re-validate the mutated payload with the same rules as create
        // (minus the alias-conflict check).
        validate_alias(&upstream.alias)?;
        if upstream.host.trim().is_empty() {
            return Err(DomainError::validation("upstream `host` must not be empty"));
        }
        if upstream.port == 0 {
            return Err(DomainError::validation("upstream `port` must be > 0"));
        }
        self.upsert_validated(upstream)?;
        self.invalidate_all();
        Ok(())
    }

    fn upsert_validated(&self, upstream: Upstream) -> Result<(), DomainError> {
        match upstream.scheme {
            UpstreamScheme::Https => {}
            UpstreamScheme::Http if self.allow_http_upstream => {}
            UpstreamScheme::Http => {
                return Err(DomainError::TransportNotPermitted(
                    upstream.alias,
                    "plaintext http upstreams are disabled (allow_http_upstream=false)".to_owned(),
                ));
            }
        }
        self.upstreams.insert(upstream.alias.clone(), upstream);
        Ok(())
    }

    /// Deletes an upstream; rejected while routes reference it.
    ///
    /// # Errors
    ///
    /// Returns `UpstreamNotFound` or `Validation` (still referenced).
    pub fn delete_upstream(&self, alias: &str) -> Result<(), DomainError> {
        let referenced = self
            .routes
            .iter()
            .any(|r| r.value().upstream_alias.as_deref() == Some(alias));
        if referenced {
            return Err(DomainError::validation(format!(
                "upstream `{alias}` is referenced by one or more routes"
            )));
        }
        // A still-bound upstream (plugins attached) must not be silently
        // deleted alongside its bindings — surface a 409 instead.
        if let Some(bindings) = self.upstream_bindings.get(alias)
            && !bindings.value().read().is_empty()
        {
            return Err(DomainError::UpstreamStillBound(alias.to_owned()));
        }
        if self.upstreams.remove(alias).is_none() {
            return Err(DomainError::UpstreamNotFound(alias.to_owned()));
        }
        self.fire_upstream_delete(alias);
        self.invalidate_all();
        Ok(())
    }

    /// Enables/disables an upstream (`inst-disable`/`inst-enable`).
    ///
    /// # Errors
    ///
    /// Returns `UpstreamNotFound` when the alias is unknown.
    pub fn set_upstream_enabled(&self, alias: &str, enabled: bool) -> Result<(), DomainError> {
        let mut u = self
            .upstreams
            .get_mut(alias)
            .ok_or_else(|| DomainError::UpstreamNotFound(alias.to_owned()))?;
        // @cpt-begin:cpt-cf-oagw-state-control-plane-enablement:ph-1:inst-disable
        u.enabled = enabled;
        // @cpt-end:cpt-cf-oagw-state-control-plane-enablement:ph-1:inst-disable
        let _ = enabled;
        drop(u);
        self.invalidate_all();
        Ok(())
    }

    // ------------------------------------------------------------------
    // Routes
    // ------------------------------------------------------------------

    /// Lists every route.
    #[must_use]
    pub fn list_routes(&self) -> Vec<Route> {
        let mut v: Vec<Route> = self.routes.iter().map(|e| e.value().clone()).collect();
        v.sort_by(|a, b| a.alias.cmp(&b.alias));
        v
    }

    /// Looks up a route by alias.
    #[must_use]
    pub fn get_route(&self, alias: &str) -> Option<Route> {
        self.routes.get(alias).map(|e| e.value().clone())
    }

    /// Creates a route after validating its upstream reference.
    ///
    /// # Errors
    ///
    /// Returns `RouteAliasConflict` / `RouteDisabledUpstream` / validation.
    pub fn upsert_route(&self, route: Route) -> Result<(), DomainError> {
        validate_alias(&route.alias)?;
        if let Some(ua) = &route.upstream_alias {
            let upstream = self.upstreams.get(ua).ok_or_else(|| {
                DomainError::RouteDisabledUpstream(route.alias.clone(), ua.clone())
            })?;
            if !upstream.enabled {
                return Err(DomainError::RouteDisabledUpstream(
                    route.alias.clone(),
                    ua.clone(),
                ));
            }
        }
        if self.routes.contains_key(&route.alias) {
            return Err(DomainError::RouteAliasConflict(route.alias));
        }
        if let Some(rate) = &route.rate_limit
            && (rate.capacity == 0 || rate.refill_per_sec <= 0.0)
        {
            return Err(DomainError::validation(
                "rate_limit requires capacity > 0 and refill_per_sec > 0",
            ));
        }
        CorsGate::validate_policy(&route.cors)?;
        self.routes.insert(route.alias.clone(), route);
        self.invalidate_all();
        Ok(())
    }

    /// Updates an existing route in place.
    ///
    /// # Errors
    ///
    /// Returns `RouteNotFound` or validation errors.
    pub fn update_route(&self, route: Route) -> Result<(), DomainError> {
        if !self.routes.contains_key(&route.alias) {
            return Err(DomainError::RouteNotFound(route.alias));
        }
        // Re-validate with the same rules as create (minus the alias-conflict
        // check) so an update cannot smuggle in an invalid payload.
        validate_alias(&route.alias)?;
        if let Some(rate) = &route.rate_limit
            && (rate.capacity == 0 || rate.refill_per_sec <= 0.0)
        {
            return Err(DomainError::validation(
                "rate_limit requires capacity > 0 and refill_per_sec > 0",
            ));
        }
        CorsGate::validate_policy(&route.cors)?;
        if let Some(ua) = &route.upstream_alias {
            let upstream = self.upstreams.get(ua).ok_or_else(|| {
                DomainError::RouteDisabledUpstream(route.alias.clone(), ua.clone())
            })?;
            if !upstream.enabled {
                return Err(DomainError::RouteDisabledUpstream(
                    route.alias.clone(),
                    ua.clone(),
                ));
            }
        }
        self.routes.insert(route.alias.clone(), route);
        self.invalidate_all();
        Ok(())
    }

    /// Deletes a route. Routes still bound to plugins are refused (409)
    /// rather than silently dropping the bindings.
    ///
    /// # Errors
    ///
    /// Returns `RouteStillBound` / `RouteNotFound`.
    pub fn delete_route(&self, alias: &str) -> Result<(), DomainError> {
        if let Some(bindings) = self.route_bindings.get(alias)
            && !bindings.value().read().is_empty()
        {
            return Err(DomainError::RouteStillBound(alias.to_owned()));
        }
        if self.routes.remove(alias).is_none() {
            return Err(DomainError::RouteNotFound(alias.to_owned()));
        }
        self.fire_route_delete(alias);
        self.invalidate_all();
        Ok(())
    }

    /// Enables/disables a route (`inst-disable`/`inst-enable`).
    ///
    /// # Errors
    ///
    /// Returns `RouteNotFound` when the alias is unknown.
    pub fn set_route_enabled(&self, alias: &str, enabled: bool) -> Result<(), DomainError> {
        let mut r = self
            .routes
            .get_mut(alias)
            .ok_or_else(|| DomainError::RouteNotFound(alias.to_owned()))?;
        // @cpt-begin:cpt-cf-oagw-state-control-plane-enablement:ph-1:inst-enable
        r.enabled = enabled;
        // @cpt-end:cpt-cf-oagw-state-control-plane-enablement:ph-1:inst-enable
        drop(r);
        self.invalidate_all();
        Ok(())
    }

    // ------------------------------------------------------------------
    // Plugins
    // ------------------------------------------------------------------

    /// Lists every plugin.
    #[must_use]
    pub fn list_plugins(&self) -> Vec<Plugin> {
        let mut v: Vec<Plugin> = self.plugins.iter().map(|e| e.value().clone()).collect();
        v.sort_by(|a, b| a.alias.cmp(&b.alias));
        v
    }

    /// Looks up a plugin by alias.
    #[must_use]
    pub fn get_plugin(&self, alias: &str) -> Option<Plugin> {
        self.plugins.get(alias).map(|e| e.value().clone())
    }

    /// Creates a plugin (`oagw_plugin`).
    ///
    /// # Errors
    ///
    /// Returns `PluginAliasConflict` or validation on unknown kind.
    pub fn upsert_plugin(&self, plugin: Plugin) -> Result<(), DomainError> {
        validate_alias(&plugin.alias)?;
        if self.plugins.contains_key(&plugin.alias) {
            return Err(DomainError::PluginAliasConflict(plugin.alias));
        }
        if plugin.kind.as_str().is_empty() {
            return Err(DomainError::validation("plugin kind is required"));
        }
        self.plugins.insert(plugin.alias.clone(), plugin);
        self.invalidate_all();
        Ok(())
    }

    /// Updates an existing plugin.
    ///
    /// # Errors
    ///
    /// Returns `PluginNotFound` when the alias is unknown.
    pub fn update_plugin(&self, plugin: Plugin) -> Result<(), DomainError> {
        if !self.plugins.contains_key(&plugin.alias) {
            return Err(DomainError::PluginNotFound(plugin.alias));
        }
        // Same alias/kind rules as create (minus the conflict check).
        validate_alias(&plugin.alias)?;
        if plugin.kind.as_str().is_empty() {
            return Err(DomainError::validation("plugin kind is required"));
        }
        self.plugins.insert(plugin.alias.clone(), plugin);
        self.invalidate_all();
        Ok(())
    }

    /// Deletes a plugin; rejected while still bound to a route or upstream.
    ///
    /// # Errors
    ///
    /// Returns `PluginStillBound` / `PluginNotFound`.
    pub fn delete_plugin(&self, alias: &str) -> Result<(), DomainError> {
        let bound_to_route = self
            .route_bindings
            .iter()
            .any(|b| b.value().read().iter().any(|p| p == alias));
        let bound_to_upstream = self
            .upstream_bindings
            .iter()
            .any(|b| b.value().read().iter().any(|p| p == alias));
        if bound_to_route || bound_to_upstream {
            return Err(DomainError::PluginStillBound(alias.to_owned()));
        }
        if self.plugins.remove(alias).is_none() {
            return Err(DomainError::PluginNotFound(alias.to_owned()));
        }
        self.invalidate_all();
        Ok(())
    }

    // ------------------------------------------------------------------
    // Bindings
    // ------------------------------------------------------------------

    /// Binds a plugin to a route (`oagw_route_plugin`).
    ///
    /// # Errors
    ///
    /// Returns not-found errors for unknown aliases.
    pub fn bind_plugin_to_route(
        &self,
        route_alias: &str,
        plugin_alias: &str,
    ) -> Result<(), DomainError> {
        if self.routes.get(route_alias).is_none() {
            return Err(DomainError::RouteNotFound(route_alias.to_owned()));
        }
        if self.plugins.get(plugin_alias).is_none() {
            return Err(DomainError::PluginNotFound(plugin_alias.to_owned()));
        }
        let list = self
            .route_bindings
            .entry(route_alias.to_owned())
            .or_insert_with(|| Arc::new(RwLock::new(Vec::new())))
            .clone();
        let mut guard = list.write();
        if guard.iter().any(|p| p == plugin_alias) {
            return Err(DomainError::PluginInUse(plugin_alias.to_owned()));
        }
        guard.push(plugin_alias.to_owned());
        drop(guard);
        self.invalidate_alias(route_alias);
        Ok(())
    }

    /// Unbinds a plugin from a route.
    ///
    /// # Errors
    ///
    /// Returns not-found errors for unknown aliases.
    pub fn unbind_plugin_from_route(
        &self,
        route_alias: &str,
        plugin_alias: &str,
    ) -> Result<(), DomainError> {
        if self.routes.get(route_alias).is_none() {
            return Err(DomainError::RouteNotFound(route_alias.to_owned()));
        }
        if let Some(list) = self.route_bindings.get(route_alias) {
            list.value().write().retain(|p| p != plugin_alias);
        }
        self.invalidate_alias(route_alias);
        Ok(())
    }

    /// Binds a plugin to an upstream (`oagw_upstream_plugin`).
    ///
    /// # Errors
    ///
    /// Returns not-found errors for unknown aliases.
    pub fn bind_plugin_to_upstream(
        &self,
        upstream_alias: &str,
        plugin_alias: &str,
    ) -> Result<(), DomainError> {
        if self.upstreams.get(upstream_alias).is_none() {
            return Err(DomainError::UpstreamNotFound(upstream_alias.to_owned()));
        }
        if self.plugins.get(plugin_alias).is_none() {
            return Err(DomainError::PluginNotFound(plugin_alias.to_owned()));
        }
        let list = self
            .upstream_bindings
            .entry(upstream_alias.to_owned())
            .or_insert_with(|| Arc::new(RwLock::new(Vec::new())))
            .clone();
        let mut guard = list.write();
        if guard.iter().any(|p| p == plugin_alias) {
            return Err(DomainError::PluginInUse(plugin_alias.to_owned()));
        }
        guard.push(plugin_alias.to_owned());
        drop(guard);
        self.invalidate_all();
        Ok(())
    }

    /// Unbinds a plugin from an upstream.
    ///
    /// # Errors
    ///
    /// Returns `UpstreamNotFound` when the target upstream is unknown.
    pub fn unbind_plugin_from_upstream(
        &self,
        upstream_alias: &str,
        plugin_alias: &str,
    ) -> Result<(), DomainError> {
        if self.upstreams.get(upstream_alias).is_none() {
            return Err(DomainError::UpstreamNotFound(upstream_alias.to_owned()));
        }
        if let Some(list) = self.upstream_bindings.get(upstream_alias) {
            list.value().write().retain(|p| p != plugin_alias);
        }
        self.invalidate_all();
        Ok(())
    }

    /// Lists plugin aliases bound to a route.
    #[must_use]
    pub fn list_route_plugins(&self, route_alias: &str) -> Vec<String> {
        match self.route_bindings.get(route_alias) {
            Some(list) => list.value().read().clone(),
            None => Vec::new(),
        }
    }

    /// Lists plugin aliases bound to an upstream.
    #[must_use]
    pub fn list_upstream_plugins(&self, upstream_alias: &str) -> Vec<String> {
        match self.upstream_bindings.get(upstream_alias) {
            Some(list) => list.value().read().clone(),
            None => Vec::new(),
        }
    }

    // ------------------------------------------------------------------
    // Invalidation
    // ------------------------------------------------------------------

    /// Registers a hook fired whenever a route is deleted (used by the data
    /// plane to prune rate-limit buckets).
    pub fn on_route_delete(&self, hook: impl Fn(&str) + Send + Sync + 'static) {
        self.route_delete_hooks.write().push(Box::new(hook));
    }

    /// Registers a hook fired whenever an upstream is deleted (used by the
    /// data plane to prune load-balancer state).
    pub fn on_upstream_delete(&self, hook: impl Fn(&str) + Send + Sync + 'static) {
        self.upstream_delete_hooks.write().push(Box::new(hook));
    }

    fn fire_route_delete(&self, alias: &str) {
        for hook in self.route_delete_hooks.read().iter() {
            hook(alias);
        }
    }

    fn fire_upstream_delete(&self, alias: &str) {
        for hook in self.upstream_delete_hooks.read().iter() {
            hook(alias);
        }
    }

    /// Invalidates the L1 entry for one alias (called by route-level writes).
    pub fn invalidate_alias(&self, alias: &str) {
        self.l1_cache.invalidate(alias);
        self.l1_cache.bump_generation();
    }

    /// Invalidates the whole L1 cache and bumps the generation so every
    /// previously-computed effective config is recomputed lazily.
    pub fn invalidate_all(&self) {
        self.l1_cache.invalidate_all();
    }

    /// Global proxy timeout for routes without an override.
    #[must_use]
    pub fn default_timeout(&self) -> Duration {
        self.default_timeout
    }
}

impl std::fmt::Debug for ControlPlaneService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ControlPlaneService")
            .field("upstreams", &self.upstreams.len())
            .field("routes", &self.routes.len())
            .field("plugins", &self.plugins.len())
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::models::{Upstream, UpstreamScheme};

    fn upstream(alias: &str) -> Upstream {
        Upstream {
            alias: alias.to_owned(),
            name: alias.to_owned(),
            host: "example.com".to_owned(),
            port: 443,
            scheme: UpstreamScheme::Https,
            path_prefix: String::new(),
            enabled: true,
            timeout_secs: 0,
        }
    }

    #[test]
    fn alias_rules_reject_invalid_aliases() {
        let svc = ControlPlaneService::new(10, Duration::from_secs(2), false);
        assert!(svc.upsert_upstream(upstream("Bad Alias")).is_err());
        assert!(svc.upsert_upstream(upstream("UPPER")).is_err());
        assert!(svc.upsert_upstream(upstream("")).is_err());
        assert!(svc.upsert_upstream(upstream("good.alias_1-x")).is_ok());
    }

    #[test]
    fn upstream_alias_conflict_maps_to_409() {
        let svc = ControlPlaneService::new(10, Duration::from_secs(2), false);
        svc.upsert_upstream(upstream("svc")).expect("seed");
        let err = svc.upsert_upstream(upstream("svc")).unwrap_err();
        assert!(matches!(err, DomainError::UpstreamAliasConflict(_)));
    }

    #[test]
    fn http_upstream_rejected_when_disabled() {
        let svc = ControlPlaneService::new(10, Duration::from_secs(2), false);
        let mut u = upstream("plain");
        u.scheme = UpstreamScheme::Http;
        let err = svc.upsert_upstream(u).unwrap_err();
        assert!(matches!(err, DomainError::TransportNotPermitted(_, _)));
    }

    fn plugin(alias: &str) -> Plugin {
        Plugin {
            alias: alias.to_owned(),
            kind: PluginKind::Noop,
            enabled: true,
            config: serde_json::json!({}),
        }
    }

    /// Bind-once rule (RF-012): binding the same plugin twice to the same
    /// target is a 409 conflict, not a silent idempotent no-op.
    #[test]
    fn bind_once_rejects_second_binding() {
        let svc = ControlPlaneService::new(10, Duration::from_secs(2), false);
        svc.upsert_upstream(upstream("svc")).expect("seed upstream");
        svc.upsert_route(crate::domain::models::Route {
            alias: "shop".to_owned(),
            upstream_alias: Some("svc".to_owned()),
            methods: None,
            http_matches: Vec::new(),
            rate_limit: None,
            cors: Default::default(),
            enabled: true,
            priority: 0,
        })
        .expect("seed route");
        svc.upsert_plugin(plugin("p1")).expect("seed plugin");

        svc.bind_plugin_to_route("shop", "p1").expect("first bind");
        let err = svc.bind_plugin_to_route("shop", "p1").unwrap_err();
        assert!(matches!(err, DomainError::PluginInUse(_)), "{err}");

        svc.bind_plugin_to_upstream("svc", "p1")
            .expect("upstream bind");
        let err = svc.bind_plugin_to_upstream("svc", "p1").unwrap_err();
        assert!(matches!(err, DomainError::PluginInUse(_)), "{err}");
    }

    /// Deleting a still-bound entity is a 409, never a silent success that
    /// drops the bindings (RF-012).
    #[test]
    fn delete_of_bound_rejected() {
        let svc = ControlPlaneService::new(10, Duration::from_secs(2), false);
        svc.upsert_upstream(upstream("svc")).expect("seed upstream");
        svc.upsert_route(crate::domain::models::Route {
            alias: "shop".to_owned(),
            upstream_alias: Some("svc".to_owned()),
            methods: None,
            http_matches: Vec::new(),
            rate_limit: None,
            cors: Default::default(),
            enabled: true,
            priority: 0,
        })
        .expect("seed route");
        svc.upsert_plugin(plugin("p1")).expect("seed plugin");
        svc.bind_plugin_to_route("shop", "p1")
            .expect("bind to route");
        svc.bind_plugin_to_upstream("svc", "p1")
            .expect("bind to upstream");

        assert!(matches!(
            svc.delete_route("shop").unwrap_err(),
            DomainError::RouteStillBound(_)
        ));
        assert!(matches!(
            svc.delete_plugin("p1").unwrap_err(),
            DomainError::PluginStillBound(_)
        ));
        assert!(matches!(
            svc.delete_upstream("svc").unwrap_err(),
            DomainError::RouteDisabledUpstream(_, _)
                | DomainError::UpstreamStillBound(_)
                | DomainError::Validation { .. }
        ));

        // After unbinding, deletion succeeds.
        svc.unbind_plugin_from_route("shop", "p1")
            .expect("unbind route");
        svc.unbind_plugin_from_upstream("svc", "p1")
            .expect("unbind upstream");
        svc.delete_route("shop")
            .expect("route deleted after unbind");
        svc.delete_upstream("svc")
            .expect("upstream deleted after unbind");
    }

    /// Prune hooks must fire on route/upstream deletion so data-plane maps
    /// (rate-limit buckets, load balancers) release per-entity state (RF-017).
    #[test]
    fn delete_hooks_fire_for_downstream_pruning() {
        use std::sync::Mutex;
        let svc = ControlPlaneService::new(10, Duration::from_secs(2), false);
        let pruned_routes: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let pruned_upstreams: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        {
            let routes = pruned_routes.clone();
            svc.on_route_delete(move |alias| routes.lock().unwrap().push(alias.to_owned()));
        }
        {
            let upstreams = pruned_upstreams.clone();
            svc.on_upstream_delete(move |alias| upstreams.lock().unwrap().push(alias.to_owned()));
        }

        svc.upsert_upstream(upstream("svc")).expect("seed upstream");
        svc.upsert_route(crate::domain::models::Route {
            alias: "shop".to_owned(),
            upstream_alias: Some("svc".to_owned()),
            methods: None,
            http_matches: Vec::new(),
            rate_limit: None,
            cors: Default::default(),
            enabled: true,
            priority: 0,
        })
        .expect("seed route");
        svc.delete_route("shop").expect("delete route");
        svc.delete_upstream("svc").expect("delete upstream");

        assert_eq!(*pruned_routes.lock().unwrap(), vec!["shop".to_owned()]);
        assert_eq!(*pruned_upstreams.lock().unwrap(), vec!["svc".to_owned()]);
    }
}
