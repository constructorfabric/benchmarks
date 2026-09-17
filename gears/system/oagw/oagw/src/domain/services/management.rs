//! `ControlPlaneService` — CRUD for upstreams, routes and plugins, alias
//! resolution, and plugin-reference validation (`DESIGN.md` §3.2).
//!
//! The service is deliberately synchronous and transport-agnostic: it owns
//! every cross-resource invariant (alias uniqueness per tenant, route match
//! uniqueness per upstream, plugin reference integrity, the upstream → route
//! cascade) while knowing nothing about HTTP or storage.
use std::sync::Arc;

use uuid::Uuid;

use crate::domain::alias;
use crate::domain::error::DomainError;
use crate::domain::gts;
use crate::domain::model::{
    AuthConfig, Endpoint, Plugin, PluginKind, PluginsConfig, Protocol, Route, RouteMatcher,
    ServerConfig, Upstream, now_millis,
};
use crate::domain::query::ListQuery;
use crate::domain::reason;
use crate::domain::repo::{ConfigStore, ControlPlaneSnapshot};

/// List pagination bounds (`DESIGN.md` §3.3 "List Query Parameters":
/// `$top` defaults to 50 with a ceiling of 100).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ListLimits {
    /// `$top` applied when the caller omits it.
    pub default_top: usize,
    /// Largest accepted `$top`.
    pub max_top: usize,
}

impl Default for ListLimits {
    fn default() -> Self {
        Self {
            default_top: 50,
            max_top: 100,
        }
    }
}

/// One built-in (named) plugin of the types-registry catalog.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BuiltInPlugin {
    /// Full GTS identifier, e.g.
    /// `gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1`.
    pub gts_id: &'static str,
    /// Short name (`apikey`, `required_headers`, ...).
    pub name: &'static str,
    /// Plugin family.
    pub kind: PluginKind,
    /// `true` when the identifier is resolvable and bindable through a
    /// `plugins.items[]` chain or the `auth` block. Catalog-only identifiers
    /// (`auth.basic`, `auth.bearer`, `guard.timeout`, `guard.cors`,
    /// `transform.logging`, `transform.metrics`) have no in-process
    /// implementation and are rejected at bind time.
    pub bindable: bool,
    /// One-line description.
    pub description: &'static str,
}

/// The built-in plugin catalog (`DESIGN.md` §3.1 "Plugin Schemas").
pub const BUILT_IN_PLUGINS: &[BuiltInPlugin] = &[
    BuiltInPlugin {
        gts_id: "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.noop.v1",
        name: "noop",
        kind: PluginKind::Auth,
        bindable: true,
        description: "No-op authentication; requests are forwarded unauthenticated",
    },
    BuiltInPlugin {
        gts_id: "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1",
        name: "apikey",
        kind: PluginKind::Auth,
        bindable: true,
        description: "Static API key injection from the credential store",
    },
    BuiltInPlugin {
        gts_id: "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.oauth2_client_cred.v1",
        name: "oauth2_client_cred",
        kind: PluginKind::Auth,
        bindable: true,
        description: "OAuth2 client-credentials token acquisition",
    },
    BuiltInPlugin {
        gts_id: "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.oauth2_client_cred_basic.v1",
        name: "oauth2_client_cred_basic",
        kind: PluginKind::Auth,
        bindable: true,
        description: "OAuth2 client credentials over HTTP Basic",
    },
    BuiltInPlugin {
        gts_id: "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.basic.v1",
        name: "basic",
        kind: PluginKind::Auth,
        bindable: false,
        description: "Catalogued only; no AuthPlugin implementation is registered",
    },
    BuiltInPlugin {
        gts_id: "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.bearer.v1",
        name: "bearer",
        kind: PluginKind::Auth,
        bindable: false,
        description: "Catalogued only; no AuthPlugin implementation is registered",
    },
    BuiltInPlugin {
        gts_id: "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1",
        name: "required_headers",
        kind: PluginKind::Guard,
        bindable: true,
        description: "Required request/response header enforcement",
    },
    BuiltInPlugin {
        gts_id: "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.timeout.v1",
        name: "timeout",
        kind: PluginKind::Guard,
        bindable: false,
        description: "Catalogued only; request timeout is core data-plane functionality",
    },
    BuiltInPlugin {
        gts_id: "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.cors.v1",
        name: "cors",
        kind: PluginKind::Guard,
        bindable: false,
        description: "Catalogued only; CORS is configured via the dedicated cors field",
    },
    BuiltInPlugin {
        gts_id: "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.request_id.v1",
        name: "request_id",
        kind: PluginKind::Transform,
        bindable: true,
        description: "X-Request-ID injection and propagation",
    },
    BuiltInPlugin {
        gts_id: "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.logging.v1",
        name: "logging",
        kind: PluginKind::Transform,
        bindable: false,
        description: "Catalogued only; logging is core data-plane instrumentation",
    },
    BuiltInPlugin {
        gts_id: "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.metrics.v1",
        name: "metrics",
        kind: PluginKind::Transform,
        bindable: false,
        description: "Catalogued only; metrics are core data-plane instrumentation",
    },
];

/// Look up a built-in plugin by its full GTS id.
#[must_use]
pub fn built_in_plugin(gts_id: &str) -> Option<&'static BuiltInPlugin> {
    BUILT_IN_PLUGINS
        .iter()
        .find(|plugin| plugin.gts_id == gts_id)
}

/// A plugin reference resolved against the built-in catalog or a tenant's own
/// custom plugin rows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedPlugin {
    /// Canonical full GTS identifier stored as `plugin_ref`.
    pub plugin_ref: String,
    /// Extracted UUID for UUID-backed (custom) plugins.
    pub plugin_uuid: Option<Uuid>,
    /// Plugin family.
    pub kind: PluginKind,
}

/// Fields of a new or fully replaced upstream.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct UpstreamSpec {
    /// Caller-supplied alias, when any.
    pub alias: Option<String>,
    /// `enabled` flag; defaults to `true`.
    pub enabled: Option<bool>,
    /// Discovery tags; cleared when omitted on a replace.
    pub tags: Option<Vec<String>>,
    /// Endpoint pool (required).
    pub server: Option<ServerConfig>,
    /// Wire protocol (required).
    pub protocol: Option<Protocol>,
    /// Authentication plugin.
    pub auth: Option<AuthConfig>,
    /// Header rewriting rules; cleared when omitted.
    pub headers: Option<crate::domain::model::HeadersConfig>,
    /// Plugin chain; cleared when omitted.
    pub plugins: Option<PluginsConfig>,
    /// Rate limit; cleared when omitted.
    pub rate_limit: Option<crate::domain::model::RateLimitConfig>,
    /// CORS policy; cleared when omitted.
    pub cors: Option<crate::domain::model::CorsConfig>,
}

/// Fields of a new or fully replaced route.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RouteSpec {
    /// Upstream the route forwards to (required on create, immutable after).
    pub upstream_id: Option<Uuid>,
    /// Route name.
    pub name: Option<String>,
    /// Discovery tags; cleared when omitted.
    pub tags: Option<Vec<String>>,
    /// Match rule (required).
    pub matcher: Option<RouteMatcher>,
    /// Ordering key; defaults to 0.
    pub priority: Option<i32>,
    /// `enabled` flag; defaults to `true`.
    pub enabled: Option<bool>,
    /// Plugin chain; cleared when omitted.
    pub plugins: Option<PluginsConfig>,
    /// Rate limit; cleared when omitted.
    pub rate_limit: Option<crate::domain::model::RateLimitConfig>,
    /// CORS policy; cleared when omitted.
    pub cors: Option<crate::domain::model::CorsConfig>,
}

/// Fields of a new custom plugin.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PluginSpec {
    /// Tenant-unique plugin name.
    pub name: Option<String>,
    /// Plugin family.
    pub kind: Option<PluginKind>,
    /// Free-text description.
    pub description: Option<String>,
    /// Configuration document.
    pub config: Option<serde_json::Value>,
    /// Sandboxed Starlark source.
    pub source: Option<String>,
}

/// The management-plane service.
pub struct ControlPlane {
    store: Arc<dyn ConfigStore>,
    limits: ListLimits,
}

impl ControlPlane {
    /// Build a control plane over `store`.
    #[must_use]
    pub fn new(store: Arc<dyn ConfigStore>, limits: ListLimits) -> Self {
        Self { store, limits }
    }

    /// The list pagination bounds in force.
    #[must_use]
    pub const fn limits(&self) -> ListLimits {
        self.limits
    }

    /// Point-in-time view of one tenant's configuration.
    ///
    /// # Errors
    ///
    /// [`DomainError::Internal`] when the store fails.
    pub fn snapshot(&self, tenant: Uuid) -> Result<ControlPlaneSnapshot, DomainError> {
        Ok(ControlPlaneSnapshot {
            upstreams: self.store.list_upstreams(tenant)?,
            routes: self.store.list_routes(tenant)?,
        })
    }

    // ------------------------------------------------------------------
    // Upstreams
    // ------------------------------------------------------------------

    /// Create an upstream.
    ///
    /// # Errors
    ///
    /// [`DomainError::Validation`] when `server` or `protocol` is missing,
    /// [`DomainError::AliasRule`] when the alias disagrees with the
    /// derivation, [`DomainError::AliasConflict`] when the derived or given
    /// alias is already taken by the tenant, [`DomainError::UnknownPluginRef`]
    /// when a plugin reference does not resolve, and
    /// [`DomainError::FieldViolation`] for shape violations.
    pub fn create_upstream(
        &self,
        tenant: Uuid,
        spec: UpstreamSpec,
    ) -> Result<Upstream, DomainError> {
        let server = spec.server.ok_or_else(|| {
            DomainError::field("server", reason::MISSING, "server.endpoints is required")
        })?;
        server.validate()?;
        let protocol = spec.protocol.ok_or_else(|| {
            DomainError::field("protocol", reason::MISSING, "protocol is required")
        })?;
        let endpoints = server.endpoints;
        let alias = alias::resolve_alias_for_create(&endpoints, spec.alias.as_deref())?;
        if self.store.upstream_alias_taken(tenant, &alias, None)? {
            return Err(DomainError::AliasConflict {
                alias,
                tenant_id: tenant,
            });
        }
        let now = now_millis();
        let mut upstream = Upstream {
            id: Uuid::new_v4(),
            tenant_id: tenant,
            alias,
            enabled: spec.enabled.unwrap_or(true),
            tags: normalize_tags(spec.tags),
            server: ServerConfig { endpoints },
            protocol,
            auth: spec.auth,
            headers: spec.headers,
            plugins: spec.plugins,
            rate_limit: spec.rate_limit,
            cors: spec.cors,
            created_at: now,
            updated_at: now,
        };
        self.resolve_upstream_plugins(tenant, &mut upstream)?;
        upstream.validate()?;
        self.store.insert_upstream(&upstream)?;
        Ok(upstream)
    }

    /// Fetch one upstream.
    ///
    /// # Errors
    ///
    /// [`DomainError::NotFound`] when the tenant owns no such upstream.
    pub fn get_upstream(&self, tenant: Uuid, id: Uuid) -> Result<Upstream, DomainError> {
        self.store
            .find_upstream(tenant, id)?
            .ok_or_else(|| not_found("upstream", gts::UPSTREAM_TYPE, &id))
    }

    /// List upstreams with filter, ordering, and paging applied.
    ///
    /// # Errors
    ///
    /// [`DomainError::Internal`] on store or serialization failure.
    pub fn list_upstreams(
        &self,
        tenant: Uuid,
        query: &ListQuery,
    ) -> Result<Vec<Upstream>, DomainError> {
        let items = self.store.list_upstreams(tenant)?;
        query.apply(items)
    }

    /// Number of upstreams matching `query`, ignoring paging.
    ///
    /// # Errors
    ///
    /// [`DomainError::Internal`] on store or serialization failure.
    pub fn count_upstreams(&self, tenant: Uuid, query: &ListQuery) -> Result<usize, DomainError> {
        let items = self.store.list_upstreams(tenant)?;
        query.count(&items)
    }

    /// Full-replace an upstream. The alias is immutable: only IP-based
    /// upstreams accept an explicit alias, and only when it matches.
    ///
    /// # Errors
    ///
    /// [`DomainError::NotFound`], [`DomainError::Validation`] when `server`
    /// or `protocol` is absent, [`DomainError::AliasRule`] when the endpoint
    /// change would move the routing key, [`DomainError::ImmutableField`]
    /// when `id` or `tenant_id` is echoed with a different value, and
    /// [`DomainError::UnknownPluginRef`] for unresolvable plugin references.
    pub fn replace_upstream(
        &self,
        tenant: Uuid,
        id: Uuid,
        spec: UpstreamSpec,
    ) -> Result<Upstream, DomainError> {
        let existing = self.get_upstream(tenant, id)?;
        let server = spec.server.ok_or_else(|| {
            DomainError::field("server", reason::MISSING, "server.endpoints is required")
        })?;
        server.validate()?;
        let protocol = spec.protocol.ok_or_else(|| {
            DomainError::field("protocol", reason::MISSING, "protocol is required")
        })?;
        alias::enforce_alias_update(&existing, &server.endpoints, spec.alias.as_deref())?;
        let mut upstream = Upstream {
            id: existing.id,
            tenant_id: existing.tenant_id,
            alias: existing.alias,
            enabled: spec.enabled.unwrap_or(true),
            tags: normalize_tags(spec.tags),
            server,
            protocol,
            auth: spec.auth,
            headers: spec.headers,
            plugins: spec.plugins,
            rate_limit: spec.rate_limit,
            cors: spec.cors,
            created_at: existing.created_at,
            updated_at: now_millis(),
        };
        self.resolve_upstream_plugins(tenant, &mut upstream)?;
        upstream.validate()?;
        self.store.replace_upstream(&upstream)?;
        Ok(upstream)
    }

    /// Delete an upstream together with every route bound to it.
    ///
    /// # Errors
    ///
    /// [`DomainError::NotFound`] when the upstream is absent.
    pub fn delete_upstream(&self, tenant: Uuid, id: Uuid) -> Result<(), DomainError> {
        let (deleted, cascaded) = self.store.delete_upstream_cascade(tenant, id)?;
        if deleted.is_none() {
            return Err(not_found("upstream", gts::UPSTREAM_TYPE, &id));
        }
        if cascaded > 0 {
            tracing::debug!(
                tenant = %tenant,
                upstream = %id,
                routes = cascaded,
                "upstream delete cascaded route rows"
            );
        }
        Ok(())
    }

    /// Enable or disable an upstream.
    ///
    /// # Errors
    ///
    /// [`DomainError::NotFound`] when the upstream is absent.
    pub fn set_upstream_enabled(
        &self,
        tenant: Uuid,
        id: Uuid,
        enabled: bool,
    ) -> Result<Upstream, DomainError> {
        let mut upstream = self.get_upstream(tenant, id)?;
        if upstream.enabled != enabled {
            upstream.enabled = enabled;
            upstream.updated_at = now_millis();
            self.store.replace_upstream(&upstream)?;
        }
        Ok(upstream)
    }

    /// The endpoint pool of an upstream.
    ///
    /// # Errors
    ///
    /// [`DomainError::NotFound`] when the upstream is absent.
    pub fn upstream_endpoints(&self, tenant: Uuid, id: Uuid) -> Result<ServerConfig, DomainError> {
        Ok(self.get_upstream(tenant, id)?.server)
    }

    /// Append one endpoint to the pool.
    ///
    /// # Errors
    ///
    /// [`DomainError::NotFound`], [`DomainError::FieldViolation`], or
    /// [`DomainError::AliasRule`] when the enlarged pool would change the
    /// derived alias.
    pub fn add_upstream_endpoint(
        &self,
        tenant: Uuid,
        id: Uuid,
        endpoint: Endpoint,
    ) -> Result<Upstream, DomainError> {
        let mut upstream = self.get_upstream(tenant, id)?;
        if upstream
            .server
            .endpoints
            .iter()
            .any(|existing| existing == &endpoint)
        {
            return Ok(upstream);
        }
        let mut endpoints = upstream.server.endpoints.clone();
        endpoints.push(endpoint);
        self.apply_endpoints(&mut upstream, endpoints)?;
        Ok(upstream)
    }

    /// Replace the whole endpoint pool.
    ///
    /// # Errors
    ///
    /// As [`ControlPlane::add_upstream_endpoint`].
    pub fn replace_upstream_endpoints(
        &self,
        tenant: Uuid,
        id: Uuid,
        endpoints: Vec<Endpoint>,
    ) -> Result<Upstream, DomainError> {
        let mut upstream = self.get_upstream(tenant, id)?;
        self.apply_endpoints(&mut upstream, endpoints)?;
        Ok(upstream)
    }

    /// Remove one endpoint from the pool by position.
    ///
    /// # Errors
    ///
    /// [`DomainError::NotFound`] when the upstream or the position is absent,
    /// [`DomainError::FieldViolation`] when the pool would become empty.
    pub fn delete_upstream_endpoint(
        &self,
        tenant: Uuid,
        id: Uuid,
        position: usize,
    ) -> Result<(), DomainError> {
        let mut upstream = self.get_upstream(tenant, id)?;
        if position >= upstream.server.endpoints.len() {
            return Err(not_found("endpoint", gts::UPSTREAM_TYPE, &id));
        }
        let mut endpoints = upstream.server.endpoints.clone();
        endpoints.remove(position);
        self.apply_endpoints(&mut upstream, endpoints)?;
        Ok(())
    }

    /// Shared tail of every pool mutation: validate, re-derive the alias
    /// rules, and persist.
    fn apply_endpoints(
        &self,
        upstream: &mut Upstream,
        endpoints: Vec<Endpoint>,
    ) -> Result<(), DomainError> {
        let server = ServerConfig {
            endpoints: endpoints.clone(),
        };
        server.validate()?;
        alias::enforce_alias_update(upstream, &endpoints, None)?;
        upstream.server = server;
        upstream.updated_at = now_millis();
        upstream.validate()?;
        self.store.replace_upstream(upstream)?;
        Ok(())
    }

    /// The ordered plugin chain of an upstream.
    ///
    /// # Errors
    ///
    /// [`DomainError::NotFound`] when the upstream is absent.
    pub fn upstream_plugins(&self, tenant: Uuid, id: Uuid) -> Result<PluginsConfig, DomainError> {
        Ok(self.get_upstream(tenant, id)?.plugins.unwrap_or_default())
    }

    /// Replace the plugin chain of an upstream, positionally renumbered from
    /// zero.
    ///
    /// # Errors
    ///
    /// [`DomainError::NotFound`] or [`DomainError::UnknownPluginRef`].
    pub fn set_upstream_plugins(
        &self,
        tenant: Uuid,
        id: Uuid,
        plugins: PluginsConfig,
    ) -> Result<Upstream, DomainError> {
        let mut upstream = self.get_upstream(tenant, id)?;
        let mut resolved = plugins;
        for binding in &mut resolved.items {
            let resolved_ref = self.resolve_plugin_ref(tenant, &binding.plugin_ref, None)?;
            binding.plugin_ref = resolved_ref.plugin_ref;
            binding.plugin_uuid = resolved_ref.plugin_uuid;
        }
        upstream.plugins = Some(resolved);
        upstream.updated_at = now_millis();
        upstream.validate()?;
        self.store.replace_upstream(&upstream)?;
        Ok(upstream)
    }

    /// Remove one chain binding by position.
    ///
    /// # Errors
    ///
    /// [`DomainError::NotFound`] when the binding position is absent.
    pub fn delete_upstream_plugin(
        &self,
        tenant: Uuid,
        id: Uuid,
        position: usize,
    ) -> Result<(), DomainError> {
        let mut upstream = self.get_upstream(tenant, id)?;
        let Some(plugins) = &mut upstream.plugins else {
            return Err(not_found("plugin binding", gts::UPSTREAM_TYPE, &id));
        };
        if position >= plugins.items.len() {
            return Err(not_found("plugin binding", gts::UPSTREAM_TYPE, &id));
        }
        plugins.items.remove(position);
        upstream.updated_at = now_millis();
        upstream.validate()?;
        self.store.replace_upstream(&upstream)?;
        Ok(())
    }

    /// Replace the auth plugin of an upstream. Passing `None` clears the
    /// auth slot.
    ///
    /// # Errors
    ///
    /// [`DomainError::NotFound`] or [`DomainError::UnknownPluginRef`].
    pub fn set_upstream_auth(
        &self,
        tenant: Uuid,
        id: Uuid,
        auth: Option<AuthConfig>,
    ) -> Result<Upstream, DomainError> {
        let mut upstream = self.get_upstream(tenant, id)?;
        if let Some(auth) = &auth {
            let resolved =
                self.resolve_plugin_ref(tenant, &auth.plugin_type, Some(PluginKind::Auth))?;
            upstream.auth = Some(AuthConfig {
                plugin_type: resolved.plugin_ref,
                plugin_uuid: resolved.plugin_uuid,
                sharing: auth.sharing,
                config: auth.config.clone(),
            });
        } else {
            upstream.auth = None;
        }
        upstream.updated_at = now_millis();
        upstream.validate()?;
        self.store.replace_upstream(&upstream)?;
        Ok(upstream)
    }

    // ------------------------------------------------------------------
    // Routes
    // ------------------------------------------------------------------

    /// Create a route. `upstream_id` must name an upstream of the calling
    /// tenant.
    ///
    /// # Errors
    ///
    /// [`DomainError::NotFound`] when the upstream is absent,
    /// [`DomainError::RouteMatchConflict`] when the match key is claimed, and
    /// [`DomainError::FieldViolation`] for shape violations.
    pub fn create_route(&self, tenant: Uuid, spec: RouteSpec) -> Result<Route, DomainError> {
        let upstream_id = spec.upstream_id.ok_or_else(|| {
            DomainError::field("upstream_id", reason::MISSING, "upstream_id is required")
        })?;
        // Ancestor upstreams are not addressable through the management API:
        // scoping the lookup to the calling tenant makes them 404.
        self.get_upstream(tenant, upstream_id)?;
        let matcher = spec
            .matcher
            .ok_or_else(|| DomainError::field("match", reason::MISSING, "match is required"))?;
        let now = now_millis();
        let mut route = Route {
            id: Uuid::new_v4(),
            tenant_id: tenant,
            upstream_id,
            name: spec.name,
            tags: normalize_tags(spec.tags),
            matcher,
            priority: spec.priority.unwrap_or_default(),
            enabled: spec.enabled.unwrap_or(true),
            plugins: spec.plugins,
            rate_limit: spec.rate_limit,
            cors: spec.cors,
            created_at: now,
            updated_at: now,
        };
        route.validate()?;
        if let Some(plugins) = &mut route.plugins {
            for binding in &mut plugins.items {
                let resolved_ref = self.resolve_plugin_ref(tenant, &binding.plugin_ref, None)?;
                binding.plugin_ref = resolved_ref.plugin_ref;
                binding.plugin_uuid = resolved_ref.plugin_uuid;
            }
        }
        self.assert_match_key_free(tenant, &route, None)?;
        self.store.insert_route(&route)?;
        Ok(route)
    }

    /// Fetch one route.
    ///
    /// # Errors
    ///
    /// [`DomainError::NotFound`] when the tenant owns no such route.
    pub fn get_route(&self, tenant: Uuid, id: Uuid) -> Result<Route, DomainError> {
        self.store
            .find_route(tenant, id)?
            .ok_or_else(|| not_found("route", gts::ROUTE_TYPE, &id))
    }

    /// List routes with filter, ordering, and paging applied.
    ///
    /// # Errors
    ///
    /// [`DomainError::Internal`] on store or serialization failure.
    pub fn list_routes(&self, tenant: Uuid, query: &ListQuery) -> Result<Vec<Route>, DomainError> {
        let items = self.store.list_routes(tenant)?;
        query.apply(items)
    }

    /// Number of routes matching `query`, ignoring paging.
    ///
    /// # Errors
    ///
    /// [`DomainError::Internal`] on store or serialization failure.
    pub fn count_routes(&self, tenant: Uuid, query: &ListQuery) -> Result<usize, DomainError> {
        let items = self.store.list_routes(tenant)?;
        query.count(&items)
    }

    /// Full-replace a route. `upstream_id` is immutable.
    ///
    /// # Errors
    ///
    /// [`DomainError::NotFound`], [`DomainError::ImmutableField`],
    /// [`DomainError::RouteMatchConflict`], [`DomainError::FieldViolation`].
    pub fn replace_route(
        &self,
        tenant: Uuid,
        id: Uuid,
        spec: RouteSpec,
    ) -> Result<Route, DomainError> {
        let existing = self.get_route(tenant, id)?;
        if let Some(upstream_id) = spec.upstream_id
            && upstream_id != existing.upstream_id
        {
            return Err(DomainError::ImmutableField {
                field: "upstream_id",
            });
        }
        let matcher = spec
            .matcher
            .ok_or_else(|| DomainError::field("match", reason::MISSING, "match is required"))?;
        let mut route = Route {
            id: existing.id,
            tenant_id: existing.tenant_id,
            upstream_id: existing.upstream_id,
            name: spec.name,
            tags: normalize_tags(spec.tags),
            matcher,
            priority: spec.priority.unwrap_or_default(),
            enabled: spec.enabled.unwrap_or(true),
            plugins: spec.plugins,
            rate_limit: spec.rate_limit,
            cors: spec.cors,
            created_at: existing.created_at,
            updated_at: now_millis(),
        };
        route.validate()?;
        if let Some(plugins) = &mut route.plugins {
            for binding in &mut plugins.items {
                let resolved_ref = self.resolve_plugin_ref(tenant, &binding.plugin_ref, None)?;
                binding.plugin_ref = resolved_ref.plugin_ref;
                binding.plugin_uuid = resolved_ref.plugin_uuid;
            }
        }
        self.assert_match_key_free(tenant, &route, Some(route.id))?;
        self.store.replace_route(&route)?;
        Ok(route)
    }

    /// Delete one route.
    ///
    /// # Errors
    ///
    /// [`DomainError::NotFound`] when the route is absent.
    pub fn delete_route(&self, tenant: Uuid, id: Uuid) -> Result<(), DomainError> {
        if self.store.delete_route(tenant, id)?.is_none() {
            return Err(not_found("route", gts::ROUTE_TYPE, &id));
        }
        Ok(())
    }

    /// Enable or disable a route.
    ///
    /// # Errors
    ///
    /// [`DomainError::NotFound`] when the route is absent.
    pub fn set_route_enabled(
        &self,
        tenant: Uuid,
        id: Uuid,
        enabled: bool,
    ) -> Result<Route, DomainError> {
        let mut route = self.get_route(tenant, id)?;
        if route.enabled != enabled {
            route.enabled = enabled;
            route.updated_at = now_millis();
            self.store.replace_route(&route)?;
        }
        Ok(route)
    }

    /// The ordered plugin chain of a route.
    ///
    /// # Errors
    ///
    /// [`DomainError::NotFound`] when the route is absent.
    pub fn route_plugins(&self, tenant: Uuid, id: Uuid) -> Result<PluginsConfig, DomainError> {
        Ok(self.get_route(tenant, id)?.plugins.unwrap_or_default())
    }

    /// Replace the plugin chain of a route.
    ///
    /// # Errors
    ///
    /// [`DomainError::NotFound`] or [`DomainError::UnknownPluginRef`].
    pub fn set_route_plugins(
        &self,
        tenant: Uuid,
        id: Uuid,
        plugins: PluginsConfig,
    ) -> Result<Route, DomainError> {
        let mut route = self.get_route(tenant, id)?;
        let mut resolved = plugins;
        for binding in &mut resolved.items {
            let resolved_ref = self.resolve_plugin_ref(tenant, &binding.plugin_ref, None)?;
            binding.plugin_ref = resolved_ref.plugin_ref;
            binding.plugin_uuid = resolved_ref.plugin_uuid;
        }
        route.plugins = Some(resolved);
        route.updated_at = now_millis();
        route.validate()?;
        self.store.replace_route(&route)?;
        Ok(route)
    }

    /// Remove one chain binding of a route by position.
    ///
    /// # Errors
    ///
    /// [`DomainError::NotFound`] when the position is absent.
    pub fn delete_route_plugin(
        &self,
        tenant: Uuid,
        id: Uuid,
        position: usize,
    ) -> Result<(), DomainError> {
        let mut route = self.get_route(tenant, id)?;
        let Some(plugins) = &mut route.plugins else {
            return Err(not_found("plugin binding", gts::ROUTE_TYPE, &id));
        };
        if position >= plugins.items.len() {
            return Err(not_found("plugin binding", gts::ROUTE_TYPE, &id));
        }
        plugins.items.remove(position);
        route.updated_at = now_millis();
        route.validate()?;
        self.store.replace_route(&route)?;
        Ok(())
    }

    /// Reject a route whose `(upstream_id, priority, match key)` tuple is
    /// already claimed by another route of the same upstream.
    fn assert_match_key_free(
        &self,
        tenant: Uuid,
        route: &Route,
        except_id: Option<Uuid>,
    ) -> Result<(), DomainError> {
        if self.store.route_match_key_taken(
            tenant,
            route.upstream_id,
            &route.uniqueness_key(),
            except_id,
        )? {
            return Err(DomainError::RouteMatchConflict {
                detail: format!(
                    "another route of upstream {} already matches {} at priority {}",
                    route.upstream_id,
                    route.matcher.match_key(),
                    route.priority
                ),
            });
        }
        Ok(())
    }

    // ------------------------------------------------------------------
    // Plugins
    // ------------------------------------------------------------------

    /// Create a custom (UUID-backed) plugin row.
    ///
    /// # Errors
    ///
    /// [`DomainError::Conflict`] when the name is taken,
    /// [`DomainError::FieldViolation`] for shape violations.
    pub fn create_plugin(&self, tenant: Uuid, spec: PluginSpec) -> Result<Plugin, DomainError> {
        let name = spec.name.ok_or_else(|| {
            DomainError::field("name", reason::MISSING, "plugin name is required")
        })?;
        let kind = spec.kind.ok_or_else(|| {
            DomainError::field("type", reason::MISSING, "plugin type is required")
        })?;
        let source = spec.source.ok_or_else(|| {
            DomainError::field("source", reason::MISSING, "plugin source is required")
        })?;
        if self.store.find_plugin_by_name(tenant, &name)?.is_some() {
            return Err(DomainError::Conflict {
                detail: format!("plugin name '{name}' already exists for this tenant"),
            });
        }
        let now = now_millis();
        let plugin = Plugin {
            id: Uuid::new_v4(),
            tenant_id: tenant,
            name,
            kind,
            description: spec.description,
            config: spec.config,
            source,
            created_at: now,
            updated_at: now,
        };
        plugin.validate()?;
        self.store.insert_plugin(&plugin)?;
        Ok(plugin)
    }

    /// Fetch one custom plugin.
    ///
    /// # Errors
    ///
    /// [`DomainError::NotFound`] when the tenant owns no such plugin.
    pub fn get_plugin(&self, tenant: Uuid, id: Uuid) -> Result<Plugin, DomainError> {
        self.store
            .find_plugin(tenant, id)?
            .ok_or_else(|| not_found("plugin", gts::PLUGIN_TYPE, &id))
    }

    /// List custom plugins with filter, ordering, and paging applied.
    ///
    /// # Errors
    ///
    /// [`DomainError::Internal`] on store or serialization failure.
    pub fn list_plugins(
        &self,
        tenant: Uuid,
        query: &ListQuery,
    ) -> Result<Vec<Plugin>, DomainError> {
        let items = self.store.list_plugins(tenant)?;
        query.apply(items)
    }

    /// Number of custom plugins matching `query`, ignoring paging.
    ///
    /// # Errors
    ///
    /// [`DomainError::Internal`] on store or serialization failure.
    pub fn count_plugins(&self, tenant: Uuid, query: &ListQuery) -> Result<usize, DomainError> {
        let items = self.store.list_plugins(tenant)?;
        query.count(&items)
    }

    /// The Starlark source of a custom plugin.
    ///
    /// # Errors
    ///
    /// [`DomainError::NotFound`] when the plugin is absent.
    pub fn plugin_source(&self, tenant: Uuid, id: Uuid) -> Result<Plugin, DomainError> {
        self.get_plugin(tenant, id)
    }

    /// Delete a custom plugin, or report who still references it.
    ///
    /// # Errors
    ///
    /// [`DomainError::NotFound`] when the plugin is absent, and
    /// [`DomainError::PluginInUse`] when an upstream or route still binds it.
    pub fn delete_plugin(&self, tenant: Uuid, id: Uuid) -> Result<(), DomainError> {
        let plugin = self.get_plugin(tenant, id)?;
        let references = self.store.plugin_references(tenant, id)?;
        if !references.is_empty() {
            return Err(DomainError::PluginInUse {
                plugin_id: plugin.gts_id(),
                references,
            });
        }
        if self.store.delete_plugin(tenant, id)?.is_none() {
            return Err(not_found("plugin", gts::PLUGIN_TYPE, &id));
        }
        Ok(())
    }

    /// Resolve a plugin reference against the built-in catalog and the
    /// tenant's own custom plugin rows, canonicalizing it to its full GTS id.
    fn resolve_plugin_ref(
        &self,
        tenant: Uuid,
        raw: &str,
        expected: Option<PluginKind>,
    ) -> Result<ResolvedPlugin, DomainError> {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Err(DomainError::UnknownPluginRef {
                detail: "plugin reference must not be empty".to_owned(),
            });
        }
        let instance = trimmed.rsplit('~').next().unwrap_or(trimmed);
        let Ok(uuid) = Uuid::parse_str(instance) else {
            return self.resolve_named_plugin(trimmed, expected);
        };
        let row =
            self.store
                .find_plugin(tenant, uuid)?
                .ok_or_else(|| DomainError::UnknownPluginRef {
                    detail: format!("no custom plugin '{trimmed}' for this tenant"),
                })?;
        if let Some(expected) = expected
            && row.kind != expected
        {
            return Err(DomainError::UnknownPluginRef {
                detail: format!(
                    "plugin '{trimmed}' is a {} plugin, but a {} plugin is required here",
                    row.kind.wire_name(),
                    expected.wire_name()
                ),
            });
        }
        // A GTS-qualified reference must agree with the stored family. The
        // base type appears with and without the `gts.` scheme prefix.
        let family = row.kind.base_type();
        if trimmed.contains('~')
            && let Some(base) = trimmed.split('~').next()
            && base != family
            && base != format!("gts.{family}")
        {
            return Err(DomainError::UnknownPluginRef {
                detail: format!(
                    "plugin '{trimmed}' names the wrong family for a {} plugin",
                    row.kind.wire_name()
                ),
            });
        }
        Ok(ResolvedPlugin {
            plugin_ref: row.kind.gts_id(&uuid),
            plugin_uuid: Some(uuid),
            kind: row.kind,
        })
    }

    /// Resolve a named (built-in) plugin reference.
    fn resolve_named_plugin(
        &self,
        raw: &str,
        expected: Option<PluginKind>,
    ) -> Result<ResolvedPlugin, DomainError> {
        let Some(entry) = built_in_plugin(raw) else {
            return Err(DomainError::UnknownPluginRef {
                detail: format!("'{raw}' is not a built-in plugin GTS id"),
            });
        };
        if !entry.bindable {
            return Err(DomainError::UnknownPluginRef {
                detail: format!(
                    "'{raw}' is catalogued only and cannot be bound: {}",
                    entry.description
                ),
            });
        }
        if let Some(expected) = expected
            && entry.kind != expected
        {
            return Err(DomainError::UnknownPluginRef {
                detail: format!(
                    "'{raw}' is a {} plugin, but a {} plugin is required here",
                    entry.kind.wire_name(),
                    expected.wire_name()
                ),
            });
        }
        Ok(ResolvedPlugin {
            plugin_ref: entry.gts_id.to_owned(),
            plugin_uuid: None,
            kind: entry.kind,
        })
    }

    /// Resolve the auth slot and the plugin chain of an upstream in place.
    fn resolve_upstream_plugins(
        &self,
        tenant: Uuid,
        upstream: &mut Upstream,
    ) -> Result<(), DomainError> {
        if let Some(auth) = upstream.auth.take() {
            let resolved_ref =
                self.resolve_plugin_ref(tenant, &auth.plugin_type, Some(PluginKind::Auth))?;
            upstream.auth = Some(AuthConfig {
                plugin_type: resolved_ref.plugin_ref,
                plugin_uuid: resolved_ref.plugin_uuid,
                sharing: auth.sharing,
                config: auth.config,
            });
        }
        if let Some(plugins) = &mut upstream.plugins {
            for binding in &mut plugins.items {
                let resolved_ref = self.resolve_plugin_ref(tenant, &binding.plugin_ref, None)?;
                binding.plugin_ref = resolved_ref.plugin_ref;
                binding.plugin_uuid = resolved_ref.plugin_uuid;
            }
        }
        Ok(())
    }
}

/// Build a tenant-scoped not-found error naming the resource kind and its
/// anonymous GTS id form.
fn not_found(kind: &str, type_id: &str, id: &Uuid) -> DomainError {
    DomainError::NotFound {
        detail: format!("no {kind} '{type_id}~{id}' for this tenant"),
    }
}

/// Lower-case and trim the tag list, dropping empties and duplicates (the
/// store keys a tag by `(parent_id, tag)`, so duplicates cannot persist).
fn normalize_tags(tags: Option<Vec<String>>) -> Vec<String> {
    let mut normalized: Vec<String> = Vec::new();
    for tag in tags.unwrap_or_default() {
        let tag = tag.trim().to_ascii_lowercase();
        if !tag.is_empty() && !normalized.contains(&tag) {
            normalized.push(tag);
        }
    }
    normalized
}

#[cfg(test)]
#[path = "management_tests.rs"]
mod tests;
