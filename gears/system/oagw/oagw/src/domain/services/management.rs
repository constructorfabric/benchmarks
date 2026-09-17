//! Control plane service: management of upstreams, routes and plugins.
//!
//! The service is deliberately **synchronous**: every operation takes `&self`,
//! never holds a lock across an `.await`, and returns plain values or
//! [`DomainError`]s. The REST handlers are thin adapters over it and the data
//! plane (part 2) reuses it for config resolution.
//!
//! Tenant scoping (DESIGN "Tenant Scoping"): every operation is scoped to the
//! calling tenant; ancestor resources are invisible (404) here and only
//! reachable through the data plane's tenant-chain walk.

use std::sync::Arc;

use crate::config::OagwConfig;
use crate::domain::dto::ListQuery;
use crate::domain::error::{DomainError, PluginReferences};
use crate::domain::model::plugin::{Plugin, PluginKind, PluginSourceRecord};
use crate::domain::model::route::{HttpMethod, Route, RouteMatch};
use crate::domain::model::upstream::{Protocol, ServerConfig, Upstream};
use crate::domain::model::{CorsConfig, HeaderRules, PluginChain, RateLimitConfig, SharingMode};
use crate::domain::services::alias::{
    AliasCandidate, AliasResolution, enforce_alias_on_create, enforce_alias_update, resolve_alias,
};

/// A validated, transport-agnostic upstream creation/replace payload.
#[derive(Debug, Clone, Default)]
pub struct UpstreamDraft {
    /// Whether the upstream accepts traffic.
    pub enabled: bool,
    /// Operator-supplied alias; `None` (or empty) means "derive".
    pub alias: Option<String>,
    /// Flat tags.
    pub tags: Vec<String>,
    /// Endpoints.
    pub server: ServerConfig,
    /// Upstream protocol.
    pub protocol: Protocol,
    /// Authentication binding.
    pub auth: Option<crate::domain::model::upstream::AuthBinding>,
    /// Header transformation rules.
    pub headers: Option<HeaderRules>,
    /// Upstream-level plugin chain.
    pub plugins: Option<PluginChain>,
    /// Upstream-level rate limit.
    pub rate_limit: Option<RateLimitConfig>,
    /// CORS configuration.
    pub cors: Option<CorsConfig>,
}

/// A transport-agnostic route creation/replace payload.
///
/// `upstream_id` is immutable on replace; when a replace carries a different
/// value the service rejects the update with 400 (see [`ManagementService::replace_route`]).
#[derive(Debug, Clone, Default)]
pub struct RouteDraft {
    /// Whether the route participates in matching.
    pub enabled: bool,
    /// Flat tags.
    pub tags: Vec<String>,
    /// Referenced upstream; required on create, immutable on replace.
    pub upstream_id: Option<uuid::Uuid>,
    /// Protocol-scoped matching rules.
    pub match_config: RouteMatch,
    /// Route-level plugin chain.
    pub plugins: Option<PluginChain>,
    /// Route-level rate limit.
    pub rate_limit: Option<RateLimitConfig>,
}

/// A transport-agnostic plugin creation payload.
#[derive(Debug, Clone, Default)]
pub struct PluginDraft {
    /// Whether the plugin may be bound.
    pub enabled: bool,
    /// Operator-facing name.
    pub name: String,
    /// Operator-facing description.
    pub description: Option<String>,
    /// Which of the three plugin kinds this is.
    pub plugin_type: PluginKind,
    /// Implementation identifier of the backing executable.
    pub implementation: Option<String>,
    /// Sharing mode.
    pub sharing: SharingMode,
    /// Flat tags.
    pub tags: Vec<String>,
    /// Default configuration.
    pub config: Option<serde_json::Value>,
    /// Declared configuration schema.
    pub config_schema: Option<serde_json::Value>,
    /// Phases the plugin participates in.
    pub phases: Vec<String>,
    /// Declared source content.
    pub source: PluginSourceRecord,
}

/// A page of resources.
#[derive(Debug, Clone, PartialEq)]
pub struct ListResult<T> {
    /// The requested page of items.
    pub items: Vec<T>,
    /// Number of items matching the filter, before pagination.
    pub total: usize,
}

/// Storage contract the control plane depends on.
///
/// Implemented in `infra/storage`; the in-memory implementation is the only
/// one today (the gear runs without a database), but the trait is the
/// extension point for a persistent backend.
pub trait ControlPlaneStore: Send + Sync {
    /// Insert an upstream, failing when the `(tenant, alias)` pair is taken.
    ///
    /// # Errors
    /// [`DomainError::AliasConflict`] when the alias is already used.
    fn insert_upstream(&self, tenant_id: uuid::Uuid, upstream: Upstream)
    -> Result<(), DomainError>;

    /// Fetch one upstream of a tenant.
    ///
    /// # Errors
    /// [`DomainError::UpstreamNotFound`] when absent.
    fn get_upstream(&self, tenant_id: uuid::Uuid, id: uuid::Uuid) -> Result<Upstream, DomainError>;

    /// Fetch one upstream of a tenant by alias (exact, case-insensitive).
    #[must_use]
    fn find_upstream_by_alias(&self, tenant_id: uuid::Uuid, alias: &str) -> Option<Upstream>;

    /// Every upstream of a tenant, ordered by id.
    #[must_use]
    fn list_upstreams(&self, tenant_id: uuid::Uuid) -> Vec<Upstream>;

    /// Replace an upstream. Must exist.
    ///
    /// # Errors
    /// [`DomainError::UpstreamNotFound`] when absent;
    /// [`DomainError::AliasConflict`] when the new alias collides.
    fn update_upstream(&self, tenant_id: uuid::Uuid, upstream: Upstream)
    -> Result<(), DomainError>;

    /// Delete an upstream. Must exist.
    ///
    /// # Errors
    /// [`DomainError::UpstreamNotFound`] when absent.
    fn delete_upstream(&self, tenant_id: uuid::Uuid, id: uuid::Uuid) -> Result<(), DomainError>;

    /// Alias candidates along a tenant chain, ordered descendant → root.
    #[must_use]
    fn alias_candidates(&self, tenant_chain: &[uuid::Uuid], alias: &str) -> Vec<AliasCandidate>;

    /// Insert a route.
    ///
    /// # Errors
    /// [`DomainError::RouteMatchConflict`] when the match duplicates another
    /// route of the same upstream.
    fn insert_route(&self, tenant_id: uuid::Uuid, route: Route) -> Result<(), DomainError>;

    /// Fetch one route of a tenant.
    ///
    /// # Errors
    /// [`DomainError::RouteNotFound`] when absent.
    fn get_route(&self, tenant_id: uuid::Uuid, id: uuid::Uuid) -> Result<Route, DomainError>;

    /// Every route of a tenant, ordered by id.
    #[must_use]
    fn list_routes(&self, tenant_id: uuid::Uuid) -> Vec<Route>;

    /// Replace a route. Must exist.
    ///
    /// # Errors
    /// [`DomainError::RouteNotFound`] when absent.
    fn update_route(&self, tenant_id: uuid::Uuid, route: Route) -> Result<(), DomainError>;

    /// Delete a route. Must exist.
    ///
    /// # Errors
    /// [`DomainError::RouteNotFound`] when absent.
    fn delete_route(&self, tenant_id: uuid::Uuid, id: uuid::Uuid) -> Result<(), DomainError>;

    /// Insert a plugin.
    ///
    /// # Errors
    /// [`DomainError::Internal`] when the id already exists.
    fn insert_plugin(&self, tenant_id: uuid::Uuid, plugin: Plugin) -> Result<(), DomainError>;

    /// Fetch one plugin of a tenant.
    ///
    /// # Errors
    /// [`DomainError::PluginNotFound`] when absent.
    fn get_plugin(&self, tenant_id: uuid::Uuid, id: uuid::Uuid) -> Result<Plugin, DomainError>;

    /// Every plugin of a tenant, ordered by id.
    #[must_use]
    fn list_plugins(&self, tenant_id: uuid::Uuid) -> Vec<Plugin>;

    /// Delete a plugin. Must exist.
    ///
    /// # Errors
    /// [`DomainError::PluginNotFound`] when absent.
    fn delete_plugin(&self, tenant_id: uuid::Uuid, id: uuid::Uuid) -> Result<(), DomainError>;

    /// Resources that still reference a plugin.
    #[must_use]
    fn plugin_references(&self, tenant_id: uuid::Uuid, plugin_id: uuid::Uuid) -> PluginReferences;

    /// Monotonic configuration generation; bumped on every write. Used by the
    /// data plane to invalidate its resolved-config caches (ADR 0005).
    #[must_use]
    fn generation(&self) -> u64;
}

/// The control plane: CRUD over upstreams, routes and plugins plus alias
/// resolution.
pub struct ManagementService {
    store: Arc<dyn ControlPlaneStore>,
    config: OagwConfig,
}

impl std::fmt::Debug for ManagementService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ManagementService")
            .field("generation", &self.store.generation())
            .finish()
    }
}

impl ManagementService {
    /// Build a control plane over `store`.
    #[must_use]
    pub fn new(store: Arc<dyn ControlPlaneStore>, config: OagwConfig) -> Self {
        Self { store, config }
    }

    /// The store backing this control plane.
    #[must_use]
    pub fn store(&self) -> &Arc<dyn ControlPlaneStore> {
        &self.store
    }

    /// Gear configuration.
    #[must_use]
    pub fn config(&self) -> &OagwConfig {
        &self.config
    }

    /// Current configuration generation.
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.store.generation()
    }

    // -- upstreams ----------------------------------------------------------

    /// Create an upstream.
    ///
    /// # Errors
    /// * [`DomainError::Validation`] — invalid endpoints, hostnames, alias or
    ///   CORS combination.
    /// * [`DomainError::AliasConflict`] — the alias is already used in the tenant.
    pub fn create_upstream(
        &self,
        tenant_id: uuid::Uuid,
        draft: UpstreamDraft,
    ) -> Result<Upstream, DomainError> {
        let mut upstream = self.build_upstream(draft)?;
        validate_upstream(&upstream)?;
        upstream.id = uuid::Uuid::new_v4();
        upstream.tenant_id = tenant_id;
        self.store.insert_upstream(tenant_id, upstream.clone())?;
        Ok(upstream)
    }

    /// Fetch an upstream.
    ///
    /// # Errors
    /// [`DomainError::UpstreamNotFound`] when the tenant has no such upstream.
    pub fn get_upstream(
        &self,
        tenant_id: uuid::Uuid,
        id: uuid::Uuid,
    ) -> Result<Upstream, DomainError> {
        self.store.get_upstream(tenant_id, id)
    }

    /// Fetch an upstream by alias.
    #[must_use]
    pub fn get_upstream_by_alias(&self, tenant_id: uuid::Uuid, alias: &str) -> Option<Upstream> {
        self.store.find_upstream_by_alias(tenant_id, alias)
    }

    /// List upstreams of a tenant.
    ///
    /// # Errors
    /// [`DomainError::Validation`] when the list query is malformed.
    pub fn list_upstreams(
        &self,
        tenant_id: uuid::Uuid,
        query: &ListQuery,
    ) -> Result<ListResult<Upstream>, DomainError> {
        list(self.store.list_upstreams(tenant_id), query)
    }

    /// Replace an upstream (full replacement; omitted optionals are cleared).
    ///
    /// The alias is immutable (ADR 0003): endpoint changes that would change
    /// the derived alias are rejected.
    ///
    /// # Errors
    /// * [`DomainError::UpstreamNotFound`] — unknown id in this tenant.
    /// * [`DomainError::AliasImmutable`] — the change would alter the alias.
    /// * [`DomainError::AliasConflict`] — the new alias collides with a
    ///   different upstream.
    /// * [`DomainError::Validation`] — invalid payload.
    pub fn replace_upstream(
        &self,
        tenant_id: uuid::Uuid,
        id: uuid::Uuid,
        draft: UpstreamDraft,
    ) -> Result<Upstream, DomainError> {
        let existing = self.store.get_upstream(tenant_id, id)?;
        let _alias = enforce_alias_update(
            &existing.alias,
            existing.endpoints(),
            &draft.server.endpoints,
            draft.alias.as_deref(),
        )?;
        let mut next = self.build_upstream(draft)?;
        validate_upstream(&next)?;
        if next.alias != existing.alias {
            // The alias is the routing key; it must never change.
            return Err(DomainError::AliasImmutable {
                detail: format!(
                    "alias is immutable: `{}` would replace `{}`",
                    next.alias, existing.alias
                ),
            });
        }
        if next.protocol != existing.protocol {
            return Err(DomainError::validation(
                "protocol",
                "upstream protocol is immutable; delete and re-create the upstream instead",
            ));
        }
        next.id = existing.id;
        next.tenant_id = tenant_id;
        next.alias = existing.alias;
        next.alias_derived = existing.alias_derived;
        self.store.update_upstream(tenant_id, next.clone())?;
        Ok(next)
    }

    /// Delete an upstream. Routes that reference it are deleted as well
    /// (`oagw_route.upstream_id` is `ON DELETE CASCADE`).
    ///
    /// # Errors
    /// [`DomainError::UpstreamNotFound`] when unknown in this tenant.
    pub fn delete_upstream(
        &self,
        tenant_id: uuid::Uuid,
        id: uuid::Uuid,
    ) -> Result<(), DomainError> {
        self.store.delete_upstream(tenant_id, id)?;
        let orphans = self
            .store
            .list_routes(tenant_id)
            .into_iter()
            .filter(|r| r.upstream_id == id)
            .collect::<Vec<_>>();
        for route in orphans {
            let _ = self.store.delete_route(tenant_id, route.id);
        }
        Ok(())
    }

    /// Resolve an alias across a tenant chain (descendant → root).
    #[must_use]
    pub fn resolve_alias(&self, tenant_chain: &[uuid::Uuid], alias: &str) -> AliasResolution {
        let candidates = self.store.alias_candidates(tenant_chain, alias);
        resolve_alias(alias, &candidates)
    }

    /// Alias candidates for an alias across a tenant chain.
    #[must_use]
    pub fn alias_candidates(
        &self,
        tenant_chain: &[uuid::Uuid],
        alias: &str,
    ) -> Vec<AliasCandidate> {
        self.store.alias_candidates(tenant_chain, alias)
    }

    fn build_upstream(&self, draft: UpstreamDraft) -> Result<Upstream, DomainError> {
        let alias = enforce_alias_on_create(&draft.server.endpoints, draft.alias.as_deref())?;
        Ok(Upstream {
            id: uuid::Uuid::nil(),
            tenant_id: uuid::Uuid::nil(),
            enabled: draft.enabled,
            alias,
            alias_derived: crate::domain::services::alias::compute_derived_alias(
                &draft.server.endpoints,
            )
            .is_derived(),
            tags: draft.tags,
            server: draft.server,
            protocol: draft.protocol,
            auth: draft.auth,
            headers: draft.headers,
            plugins: draft.plugins,
            rate_limit: draft.rate_limit,
            cors: draft.cors,
            annotations: Default::default(),
        })
    }

    // -- routes -------------------------------------------------------------

    /// Create a route.
    ///
    /// # Errors
    /// * [`DomainError::Validation`] — unknown `upstream_id`, invalid match, or
    ///   a CORS configuration that is not expressible.
    /// * [`DomainError::RouteMatchConflict`] — the match duplicates an existing
    ///   route of the same upstream.
    pub fn create_route(
        &self,
        tenant_id: uuid::Uuid,
        draft: RouteDraft,
    ) -> Result<Route, DomainError> {
        let upstream_id = draft.upstream_id.ok_or_else(|| {
            DomainError::validation("upstream_id", "upstream_id is required to create a route")
        })?;
        self.ensure_route_upstream(tenant_id, upstream_id)?;
        validate_route_match(&draft.match_config)?;
        let route = Route {
            id: uuid::Uuid::new_v4(),
            tenant_id,
            enabled: draft.enabled,
            tags: draft.tags,
            upstream_id,
            match_config: draft.match_config,
            plugins: draft.plugins,
            rate_limit: draft.rate_limit,
        };
        self.ensure_no_match_conflict(tenant_id, &route)?;
        self.store.insert_route(tenant_id, route.clone())?;
        Ok(route)
    }

    /// Fetch a route.
    ///
    /// # Errors
    /// [`DomainError::RouteNotFound`] when the tenant has no such route.
    pub fn get_route(&self, tenant_id: uuid::Uuid, id: uuid::Uuid) -> Result<Route, DomainError> {
        self.store.get_route(tenant_id, id)
    }

    /// List routes of a tenant.
    ///
    /// # Errors
    /// [`DomainError::Validation`] when the list query is malformed.
    pub fn list_routes(
        &self,
        tenant_id: uuid::Uuid,
        query: &ListQuery,
    ) -> Result<ListResult<Route>, DomainError> {
        list(self.store.list_routes(tenant_id), query)
    }

    /// Replace a route (full replacement; omitted optionals are cleared).
    ///
    /// `upstream_id` is immutable: when `draft.upstream_id` is present and
    /// differs from the stored value the update is rejected with 400.
    ///
    /// # Errors
    /// * [`DomainError::RouteNotFound`] — unknown id in this tenant.
    /// * [`DomainError::UpstreamIdImmutable`] — the payload changes
    ///   `upstream_id`.
    /// * [`DomainError::RouteMatchConflict`] — the new match collides.
    /// * [`DomainError::Validation`] — invalid payload.
    pub fn replace_route(
        &self,
        tenant_id: uuid::Uuid,
        id: uuid::Uuid,
        draft: RouteDraft,
    ) -> Result<Route, DomainError> {
        let existing = self.store.get_route(tenant_id, id)?;
        if let Some(next_upstream) = draft.upstream_id
            && next_upstream != existing.upstream_id
        {
            return Err(DomainError::UpstreamIdImmutable {
                detail: format!(
                    "route {} is bound to upstream {}; upstream_id is immutable",
                    existing.id, existing.upstream_id
                ),
            });
        }
        validate_route_match(&draft.match_config)?;
        let next = Route {
            id: existing.id,
            tenant_id,
            enabled: draft.enabled,
            tags: draft.tags,
            upstream_id: existing.upstream_id,
            match_config: draft.match_config,
            plugins: draft.plugins,
            rate_limit: draft.rate_limit,
        };
        self.ensure_no_match_conflict(tenant_id, &next)?;
        self.store.update_route(tenant_id, next.clone())?;
        Ok(next)
    }

    /// Delete a route.
    ///
    /// # Errors
    /// [`DomainError::RouteNotFound`] when unknown in this tenant.
    pub fn delete_route(&self, tenant_id: uuid::Uuid, id: uuid::Uuid) -> Result<(), DomainError> {
        self.store.delete_route(tenant_id, id)
    }

    fn ensure_route_upstream(
        &self,
        tenant_id: uuid::Uuid,
        upstream_id: uuid::Uuid,
    ) -> Result<(), DomainError> {
        match self.store.get_upstream(tenant_id, upstream_id) {
            Ok(_) => Ok(()),
            Err(DomainError::UpstreamNotFound { .. }) => Err(DomainError::validation(
                "upstream_id",
                format!("upstream {upstream_id} does not exist in this tenant"),
            )),
            Err(err) => Err(err),
        }
    }

    fn ensure_no_match_conflict(
        &self,
        tenant_id: uuid::Uuid,
        route: &Route,
    ) -> Result<(), DomainError> {
        let conflict = self.store.list_routes(tenant_id).into_iter().find(|other| {
            other.id != route.id
                && other.upstream_id == route.upstream_id
                && overlaps(&other.match_config, &route.match_config)
        });
        match conflict {
            None => Ok(()),
            Some(other) => Err(DomainError::RouteMatchConflict {
                detail: format!(
                    "route {} already matches `{}` for upstream {}",
                    other.id,
                    other.match_key(),
                    route.upstream_id
                ),
            }),
        }
    }

    // -- plugins ------------------------------------------------------------

    /// Create a custom (UUID-backed) plugin.
    ///
    /// # Errors
    /// * [`DomainError::Validation`] — missing name or unknown plugin type.
    /// * [`DomainError::Internal`] — the generated id already exists.
    pub fn create_plugin(
        &self,
        tenant_id: uuid::Uuid,
        draft: PluginDraft,
    ) -> Result<Plugin, DomainError> {
        let mut plugin = Plugin {
            id: uuid::Uuid::new_v4(),
            tenant_id,
            enabled: draft.enabled,
            name: draft.name,
            description: draft.description,
            plugin_type: draft.plugin_type,
            implementation: draft.implementation,
            sharing: draft.sharing,
            tags: draft.tags,
            config: draft.config,
            config_schema: draft.config_schema,
            phases: draft.phases,
            source: draft.source,
        };
        if plugin.name.trim().is_empty() {
            return Err(DomainError::validation("name", "plugin name is required"));
        }
        plugin.name = plugin.name.trim().to_owned();
        self.store.insert_plugin(tenant_id, plugin.clone())?;
        Ok(plugin)
    }

    /// Fetch a plugin.
    ///
    /// # Errors
    /// [`DomainError::PluginNotFound`] when the tenant has no such plugin.
    pub fn get_plugin(&self, tenant_id: uuid::Uuid, id: uuid::Uuid) -> Result<Plugin, DomainError> {
        self.store.get_plugin(tenant_id, id)
    }

    /// List plugins of a tenant.
    ///
    /// # Errors
    /// [`DomainError::Validation`] when the list query is malformed.
    pub fn list_plugins(
        &self,
        tenant_id: uuid::Uuid,
        query: &ListQuery,
    ) -> Result<ListResult<Plugin>, DomainError> {
        list(self.store.list_plugins(tenant_id), query)
    }

    /// Delete a plugin, refusing while it is still bound.
    ///
    /// # Errors
    /// * [`DomainError::PluginNotFound`] — unknown id in this tenant.
    /// * [`DomainError::PluginInUse`] — at least one upstream or route still
    ///   references the plugin.
    pub fn delete_plugin(&self, tenant_id: uuid::Uuid, id: uuid::Uuid) -> Result<(), DomainError> {
        let plugin = self.store.get_plugin(tenant_id, id)?;
        let referenced_by = self.store.plugin_references(tenant_id, id);
        if !referenced_by.is_empty() {
            return Err(DomainError::PluginInUse {
                plugin_id: plugin.gts_id(),
                referenced_by,
            });
        }
        self.store.delete_plugin(tenant_id, id)
    }

    /// The declared source / configuration of a plugin.
    ///
    /// # Errors
    /// [`DomainError::PluginNotFound`] when the tenant has no such plugin.
    pub fn get_plugin_source(
        &self,
        tenant_id: uuid::Uuid,
        id: uuid::Uuid,
    ) -> Result<PluginSourceRecord, DomainError> {
        Ok(self.store.get_plugin(tenant_id, id)?.source)
    }

    /// Resources referencing a plugin.
    #[must_use]
    pub fn plugin_references(&self, tenant_id: uuid::Uuid, id: uuid::Uuid) -> PluginReferences {
        self.store.plugin_references(tenant_id, id)
    }
}

fn list<T>(items: Vec<T>, query: &ListQuery) -> Result<ListResult<T>, DomainError>
where
    T: serde::Serialize + serde::de::DeserializeOwned,
{
    let total = items.len();
    let mut values = Vec::with_capacity(items.len());
    for item in &items {
        values.push(
            serde_json::to_value(item).map_err(|e| DomainError::Internal {
                diagnostic: format!("list projection failed: {e}"),
            })?,
        );
    }
    let page = query.apply(values);
    let mut out = Vec::with_capacity(page.len());
    for value in page {
        out.push(
            serde_json::from_value(value).map_err(|e| DomainError::Internal {
                diagnostic: format!("list projection failed: {e}"),
            })?,
        );
    }
    Ok(ListResult { items: out, total })
}

/// True when two route matches would both claim the same request.
#[must_use]
pub fn overlaps(a: &RouteMatch, b: &RouteMatch) -> bool {
    match (a, b) {
        (RouteMatch::Http(x), RouteMatch::Http(y)) => {
            x.path == y.path && x.methods.iter().any(|m| y.methods.contains(m))
        }
        (RouteMatch::Grpc(x), RouteMatch::Grpc(y)) => {
            x.service == y.service && x.method == y.method
        }
        _ => false,
    }
}

/// Validate an upstream payload.
///
/// # Errors
/// [`DomainError::Validation`] describing the first problem found.
pub fn validate_upstream(upstream: &Upstream) -> Result<(), DomainError> {
    if upstream.server.endpoints.is_empty() {
        return Err(DomainError::validation(
            "server.endpoints",
            "at least one endpoint is required",
        ));
    }
    if let Some(alias) = upstream
        .auth
        .as_ref()
        .map(|a| &a.plugin)
        .map(|p| &p.plugin_ref)
        && alias.trim().is_empty()
    {
        return Err(DomainError::validation(
            "auth.plugin_type",
            "auth.plugin_type is required when auth is configured",
        ));
    }
    for (idx, endpoint) in upstream.server.endpoints.iter().enumerate() {
        let path = format!("server.endpoints[{idx}]");
        if endpoint.host.trim().is_empty() {
            return Err(DomainError::validation(
                format!("{path}.host"),
                "host is required",
            ));
        }
        if !endpoint.is_ip_literal()
            && !crate::domain::services::alias::is_valid_hostname(&endpoint.host)
        {
            return Err(DomainError::validation(
                format!("{path}.host"),
                format!("`{}` is not a valid RFC 1123 hostname", endpoint.host),
            ));
        }
        if let Some(port) = endpoint.port
            && port == 0
        {
            return Err(DomainError::validation(
                format!("{path}.port"),
                "port must be between 1 and 65535",
            ));
        }
    }
    let mut seen = std::collections::BTreeSet::new();
    for endpoint in &upstream.server.endpoints {
        let key = (
            endpoint.scheme,
            endpoint.effective_port(),
            crate::domain::model::upstream::normalize_host(&endpoint.host),
        );
        if !seen.insert(key) {
            return Err(DomainError::validation(
                "server.endpoints",
                "duplicate endpoint (scheme, host and port are identical)",
            ));
        }
    }
    let schemes = upstream
        .server
        .endpoints
        .iter()
        .map(|e| e.scheme)
        .collect::<std::collections::BTreeSet<_>>();
    if schemes.len() > 1 {
        return Err(DomainError::validation(
            "server.endpoints",
            "all endpoints of an upstream must share the same scheme",
        ));
    }
    if let Some(cors) = &upstream.cors {
        validate_cors(cors)?;
    }
    for tag in &upstream.tags {
        if !is_tag_valid(tag) {
            return Err(DomainError::validation(
                "tags",
                format!("tag `{tag}` must match `^[a-z0-9_-]+$`"),
            ));
        }
    }
    Ok(())
}

/// Validate a tag against `^[a-z0-9_-]+$`.
#[must_use]
pub fn is_tag_valid(tag: &str) -> bool {
    !tag.is_empty()
        && tag
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
}

/// Validate a CORS configuration (ADR 0004): credentials may not be combined
/// with a wildcard origin, method or header.
///
/// # Errors
/// [`DomainError::Validation`] describing the offending list.
pub fn validate_cors(cors: &CorsConfig) -> Result<(), DomainError> {
    if !cors.enabled {
        return Ok(());
    }
    if cors.allow_credentials {
        if cors.allowed_origins.iter().any(|o| o == "*") {
            return Err(DomainError::validation(
                "cors",
                "allow_credentials cannot be combined with the wildcard origin `*`",
            ));
        }
        if cors
            .allowed_methods
            .iter()
            .any(|m| m.eq_ignore_ascii_case("*"))
        {
            return Err(DomainError::validation(
                "cors",
                "allow_credentials cannot be combined with the wildcard method `*`",
            ));
        }
        if cors
            .expose_headers
            .iter()
            .any(|h| h.eq_ignore_ascii_case("*"))
        {
            return Err(DomainError::validation(
                "cors",
                "allow_credentials cannot be combined with a wildcard exposed header",
            ));
        }
    }
    for method in &cors.allowed_methods {
        if !is_cors_method(method) {
            return Err(DomainError::validation(
                "cors.allowed_methods",
                format!("`{method}` is not a valid CORS method"),
            ));
        }
    }
    for origin in &cors.allowed_origins {
        if origin == "*" {
            continue;
        }
        if origin.parse::<url::Url>().is_err() {
            return Err(DomainError::validation(
                "cors.allowed_origins",
                format!("`{origin}` is neither `*` nor a valid URI"),
            ));
        }
    }
    Ok(())
}

/// True for a method the CORS schema permits.
#[must_use]
pub fn is_cors_method(method: &str) -> bool {
    matches!(
        method.to_ascii_uppercase().as_str(),
        "GET" | "POST" | "PUT" | "PATCH" | "DELETE" | "HEAD" | "OPTIONS"
    )
}

/// Validate a route match.
///
/// # Errors
/// [`DomainError::Validation`] describing the first problem found.
pub fn validate_route_match(match_config: &RouteMatch) -> Result<(), DomainError> {
    match match_config {
        RouteMatch::Http(http) => {
            if http.methods.is_empty() {
                return Err(DomainError::validation(
                    "match.http.methods",
                    "at least one HTTP method is required",
                ));
            }
            if http.path.trim().is_empty() {
                return Err(DomainError::validation(
                    "match.http.path",
                    "match.http.path is required",
                ));
            }
            if !http.path.starts_with('/') {
                return Err(DomainError::validation(
                    "match.http.path",
                    format!("match.http.path `{}` must start with `/`", http.path),
                ));
            }
            Ok(())
        }
        RouteMatch::Grpc(grpc) => {
            if grpc.service.trim().is_empty() {
                return Err(DomainError::validation(
                    "match.grpc.service",
                    "match.grpc.service is required",
                ));
            }
            if grpc.method.trim().is_empty() {
                return Err(DomainError::validation(
                    "match.grpc.method",
                    "match.grpc.method is required",
                ));
            }
            Ok(())
        }
    }
}

/// Validate that a method is one of the five route methods.
#[must_use]
pub fn is_route_method(method: &str) -> Option<HttpMethod> {
    HttpMethod::from_str_ci(method)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::model::upstream::Endpoint;
    use crate::infra::storage::memory::MemoryStorage;

    fn service() -> ManagementService {
        ManagementService::new(Arc::new(MemoryStorage::new()), OagwConfig::default())
    }

    fn draft(alias: Option<&str>, hosts: &[&str]) -> UpstreamDraft {
        UpstreamDraft {
            enabled: true,
            alias: alias.map(str::to_owned),
            tags: vec!["llm".to_owned()],
            server: ServerConfig {
                endpoints: hosts
                    .iter()
                    .map(|h| Endpoint {
                        scheme: crate::domain::model::Scheme::Https,
                        host: (*h).to_owned(),
                        port: None,
                    })
                    .collect(),
            },
            protocol: Protocol::Http,
            auth: None,
            headers: None,
            plugins: None,
            rate_limit: None,
            cors: None,
        }
    }

    #[test]
    fn create_upstream_derives_alias() {
        let svc = service();
        let tenant = uuid::Uuid::new_v4();
        let created = svc
            .create_upstream(tenant, draft(None, &["api.openai.com"]))
            .unwrap();
        assert_eq!(created.alias, "api.openai.com");
        assert!(created.alias_derived);
        assert_eq!(
            svc.list_upstreams(
                tenant,
                &ListQuery::new(None, None, None, None, None, 50, 100).unwrap()
            )
            .unwrap()
            .total,
            1
        );
    }

    #[test]
    fn create_upstream_rejects_alias_conflict() {
        let svc = service();
        let tenant = uuid::Uuid::new_v4();
        svc.create_upstream(tenant, draft(Some("my-service"), &["10.0.0.1"]))
            .unwrap();
        let err = svc
            .create_upstream(tenant, draft(Some("my-service"), &["10.0.0.2"]))
            .unwrap_err();
        assert_eq!(err.status(), 409);
    }

    #[test]
    fn alias_scoping_is_per_tenant() {
        let svc = service();
        let a = uuid::Uuid::new_v4();
        let b = uuid::Uuid::new_v4();
        svc.create_upstream(a, draft(Some("my-service"), &["10.0.0.1"]))
            .unwrap();
        svc.create_upstream(b, draft(Some("my-service"), &["10.0.0.2"]))
            .unwrap();
        assert!(svc.get_upstream_by_alias(a, "my-service").is_some());
        assert!(svc.get_upstream_by_alias(b, "my-service").is_some());
    }

    #[test]
    fn replace_rejects_alias_change() {
        let svc = service();
        let tenant = uuid::Uuid::new_v4();
        let created = svc
            .create_upstream(tenant, draft(None, &["api.openai.com"]))
            .unwrap();
        let err = svc
            .replace_upstream(tenant, created.id, draft(None, &["api.other.com"]))
            .unwrap_err();
        assert_eq!(err.status(), 400);
    }

    #[test]
    fn route_upstream_must_exist() {
        let svc = service();
        let tenant = uuid::Uuid::new_v4();
        let err = svc
            .create_route(
                tenant,
                RouteDraft {
                    enabled: true,
                    tags: Vec::new(),
                    upstream_id: Some(uuid::Uuid::new_v4()),
                    match_config: RouteMatch::Http(crate::domain::model::route::HttpMatch {
                        methods: vec![HttpMethod::Get],
                        path: "/v1/chat".to_owned(),
                        query_allowlist: Vec::new(),
                        path_suffix_mode: Default::default(),
                    }),
                    plugins: None,
                    rate_limit: None,
                },
            )
            .unwrap_err();
        assert_eq!(err.status(), 400);
    }

    #[test]
    fn delete_upstream_cascades_routes() {
        let svc = service();
        let tenant = uuid::Uuid::new_v4();
        let up = svc
            .create_upstream(tenant, draft(None, &["api.openai.com"]))
            .unwrap();
        let route = svc
            .create_route(
                tenant,
                RouteDraft {
                    enabled: true,
                    tags: Vec::new(),
                    upstream_id: Some(up.id),
                    match_config: RouteMatch::Http(crate::domain::model::route::HttpMatch {
                        methods: vec![HttpMethod::Get],
                        path: "/v1/chat".to_owned(),
                        query_allowlist: Vec::new(),
                        path_suffix_mode: Default::default(),
                    }),
                    plugins: None,
                    rate_limit: None,
                },
            )
            .unwrap();
        svc.delete_upstream(tenant, up.id).unwrap();
        assert!(svc.get_route(tenant, route.id).is_err());
    }

    #[test]
    fn cors_credentials_and_wildcard_are_rejected() {
        let mut cors = CorsConfig {
            sharing: SharingMode::Private,
            enabled: true,
            allowed_origins: vec!["*".to_owned()],
            allowed_methods: Vec::new(),
            expose_headers: Vec::new(),
            allow_credentials: true,
        };
        assert_eq!(validate_cors(&cors).unwrap_err().status(), 400);
        cors.allowed_origins = vec!["https://app.example.com".to_owned()];
        assert!(validate_cors(&cors).is_ok());
        cors.allowed_methods = vec!["*".to_owned()];
        assert!(validate_cors(&cors).is_err());
    }
}
