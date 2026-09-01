//! Control-plane service: the business rules of the management API
//! (DESIGN §3.3 "CRUD Semantics", "Tenant Scoping").
//!
//! The service is transport-free: it accepts and returns domain entities. The
//! REST layer (`api::rest`) owns the wire shapes. All operations are strictly
//! tenant-scoped — ancestors are invisible and not addressable.

use std::sync::Arc;

use uuid::Uuid;

use crate::domain::alias::{alias_is_valid, enforce_alias_update, normalize_alias};
use crate::domain::error::{DomainError, ReferencedBy, ResourceKind};
use crate::domain::models::{
    CorsConfig, Endpoint, GrpcMatch, MatchConfig, Plugin, RateLimitConfig, Route, ServerConfig,
    Upstream, UPSTREAM_TYPE,
};
use crate::domain::plugin::builtins::builtin_plugin_is_bindable;
use crate::domain::repo::ControlPlaneStore;
use crate::domain::time::now_millis;

/// Payload of `POST /upstreams` / `PUT /upstreams/{id}`.
#[derive(Debug, Clone, PartialEq)]
pub struct UpstreamDraft {
    /// Operator-supplied alias; derived from the endpoints when absent.
    pub alias: Option<String>,
    /// Whether the upstream accepts proxy traffic.
    pub enabled: bool,
    /// Upstream protocol.
    pub protocol: crate::domain::models::Protocol,
    /// Endpoint pool.
    pub server: ServerConfig,
    /// Authentication plugin binding.
    pub auth: Option<crate::domain::models::AuthConfig>,
    /// Header transformation rules.
    pub headers: Option<crate::domain::models::HeadersConfig>,
    /// Plugin chain binding.
    pub plugins: Option<crate::domain::models::PluginsConfig>,
    /// Upstream-level rate limit.
    pub rate_limit: Option<RateLimitConfig>,
    /// CORS configuration.
    pub cors: Option<CorsConfig>,
    /// Discovery tags.
    pub tags: Vec<String>,
}

/// Payload of `POST /routes`.
#[derive(Debug, Clone, PartialEq)]
pub struct RouteDraft {
    /// Referenced upstream; must belong to the calling tenant.
    pub upstream_id: Uuid,
    /// Whether the route participates in matching.
    pub enabled: bool,
    /// Sort key for deterministic matching.
    pub priority: i32,
    /// Match rules.
    pub match_config: MatchConfig,
    /// Route-level plugin chain.
    pub plugins: Option<crate::domain::models::PluginsConfig>,
    /// Route-level rate limit.
    pub rate_limit: Option<RateLimitConfig>,
    /// Discovery tags.
    pub tags: Vec<String>,
}

/// Payload of `PUT /routes/{id}` — `upstream_id` is immutable and therefore
/// absent.
#[derive(Debug, Clone, PartialEq)]
pub struct RouteUpdate {
    /// Whether the route participates in matching.
    pub enabled: bool,
    /// Sort key for deterministic matching.
    pub priority: i32,
    /// Match rules.
    pub match_config: MatchConfig,
    /// Route-level plugin chain.
    pub plugins: Option<crate::domain::models::PluginsConfig>,
    /// Route-level rate limit.
    pub rate_limit: Option<RateLimitConfig>,
    /// Discovery tags.
    pub tags: Vec<String>,
}

/// Payload of `POST /plugins`.
#[derive(Debug, Clone, PartialEq)]
pub struct PluginDraft {
    /// Plugin family.
    pub plugin_type: crate::domain::models::PluginKind,
    /// Unique name within the tenant.
    pub name: String,
    /// Optional description.
    pub description: Option<String>,
    /// Phases implemented by a transform plugin.
    pub phases: Vec<crate::domain::models::PluginPhase>,
    /// JSON Schema validating the plugin configuration.
    pub config_schema: Option<serde_json::Value>,
    /// Sandboxed Starlark source.
    pub source_code: String,
}

/// Control-plane operations over upstreams, routes and plugins.
pub struct ControlPlaneService {
    store: Arc<dyn ControlPlaneStore>,
    allow_http_upstream: bool,
    plugin_gc_ttl_secs: u64,
}

impl ControlPlaneService {
    /// Builds a service over the given store.
    #[must_use]
    pub fn new(store: Arc<dyn ControlPlaneStore>, config: &crate::config::OagwConfig) -> Self {
        Self {
            store,
            allow_http_upstream: config.allow_http_upstream,
            plugin_gc_ttl_secs: config.plugin_gc_ttl_secs,
        }
    }

    /// Builds a service with explicit settings (tests, embedded hosts).
    #[must_use]
    pub fn with_settings(
        store: Arc<dyn ControlPlaneStore>,
        allow_http_upstream: bool,
        plugin_gc_ttl_secs: u64,
    ) -> Self {
        Self {
            store,
            allow_http_upstream,
            plugin_gc_ttl_secs,
        }
    }

    /// Whether plaintext upstream endpoints are accepted.
    #[must_use]
    pub fn allows_http_upstream(&self) -> bool {
        self.allow_http_upstream
    }

    // ------------------------------------------------------------------
    // Upstreams
    // ------------------------------------------------------------------

    /// Creates an upstream, deriving or validating its alias.
    ///
    /// # Errors
    ///
    /// Returns [`DomainError::ValidationError`] for malformed pools, aliases,
    /// plugin bindings or configuration blocks,
    /// [`DomainError::AliasConflict`] when the alias is taken, and
    /// [`DomainError::AliasNotDerivable`] when the pool cannot produce an
    /// alias and none was supplied.
    pub async fn create_upstream(
        &self,
        tenant_id: Uuid,
        draft: UpstreamDraft,
    ) -> Result<Upstream, DomainError> {
        Self::validate_upstream_shape(
            self.allow_http_upstream,
            &draft.server,
            draft.auth.as_ref(),
            draft.plugins.as_ref(),
            draft.rate_limit.as_ref(),
            draft.cors.as_ref(),
            &draft.tags,
        )?;

        let endpoints: Vec<Endpoint> = draft.server.endpoints.clone();
        let alias = Self::resolve_new_alias(draft.alias.as_deref(), &endpoints)?;

        if let Some(existing) = self.store.find_upstream_by_alias(tenant_id, &alias).await? {
            return Err(DomainError::AliasConflict {
                alias,
                existing_upstream_id: existing.id,
            });
        }

        let now = now_millis();
        let upstream = Upstream {
            id: Uuid::new_v4(),
            tenant_id,
            alias,
            enabled: draft.enabled,
            protocol: draft.protocol,
            server: draft.server,
            auth: draft.auth,
            headers: draft.headers,
            plugins: draft.plugins,
            rate_limit: draft.rate_limit,
            cors: draft.cors,
            tags: draft.tags,
            created_at: now,
            updated_at: now,
        };
        self.store.insert_upstream(upstream.clone()).await?;
        Ok(upstream)
    }

    /// Loads a single upstream.
    ///
    /// # Errors
    ///
    /// Returns [`DomainError::NotFound`] when the upstream does not belong to
    /// `tenant_id`.
    pub async fn get_upstream(&self, tenant_id: Uuid, id: Uuid) -> Result<Upstream, DomainError> {
        self.require_upstream(tenant_id, id).await
    }

    /// Lists the upstreams of a tenant.
    ///
    /// # Errors
    ///
    /// Returns a [`DomainError`] on storage failure.
    pub async fn list_upstreams(&self, tenant_id: Uuid) -> Result<Vec<Upstream>, DomainError> {
        self.store.list_upstreams(tenant_id).await
    }

    /// Replaces an upstream (full replacement semantics).
    ///
    /// # Errors
    ///
    /// Returns [`DomainError::NotFound`] for foreign or missing ids,
    /// [`DomainError::AliasMismatch`] when a hostname-derived alias no longer
    /// matches the new pool, and [`DomainError::AliasConflict`] when a new
    /// explicit alias collides with another upstream.
    pub async fn replace_upstream(
        &self,
        tenant_id: Uuid,
        id: Uuid,
        draft: UpstreamDraft,
    ) -> Result<Upstream, DomainError> {
        let existing = self.require_upstream(tenant_id, id).await?;
        Self::validate_upstream_shape(
            self.allow_http_upstream,
            &draft.server,
            draft.auth.as_ref(),
            draft.plugins.as_ref(),
            draft.rate_limit.as_ref(),
            draft.cors.as_ref(),
            &draft.tags,
        )?;

        let new_endpoints: Vec<Endpoint> = draft.server.endpoints.clone();
        let old_endpoints: Vec<Endpoint> = existing.server.endpoints.clone();

        let requested_alias = draft.alias.as_deref().map_or_else(
            || existing.alias.clone(),
            normalize_alias,
        );
        if !alias_is_valid(&requested_alias) {
            return Err(DomainError::validation_with_value(
                "alias does not match the required pattern",
                requested_alias,
            ));
        }

        enforce_alias_update(
            &existing.alias,
            &old_endpoints,
            draft.alias.as_deref(),
            &new_endpoints,
        )?;

        if requested_alias != existing.alias
            && let Some(holder) = self
                .store
                .find_upstream_by_alias(tenant_id, &requested_alias)
                .await?
            && holder.id != existing.id
        {
            return Err(DomainError::AliasConflict {
                alias: requested_alias.clone(),
                existing_upstream_id: holder.id,
            });
        }

        let mut updated = existing.clone();
        updated.alias = requested_alias;
        updated.enabled = draft.enabled;
        updated.protocol = draft.protocol;
        updated.server = draft.server;
        updated.auth = draft.auth;
        updated.headers = draft.headers;
        updated.plugins = draft.plugins;
        updated.rate_limit = draft.rate_limit;
        updated.cors = draft.cors;
        updated.tags = draft.tags;
        updated.updated_at = now_millis();
        self.store.update_upstream(updated.clone()).await?;
        Ok(updated)
    }

    /// Deletes an upstream together with every route that references it.
    ///
    /// Review evidence (privilege boundary — tenant scoping):
    /// * Guardrail: DESIGN §3.3 "Tenant Scoping" — every operation is scoped to
    ///   the caller's tenant; ancestors are invisible and not addressable.
    /// * Rationale: the cascade reads the routes *of the calling tenant only*,
    ///   so a tenant can never delete (or even observe) a route that belongs to
    ///   another tenant, and `store.delete_*` re-checks `tenant_id` itself.
    /// * Validation performed: `delete_upstream_cascades_its_routes`,
    ///   `lists_are_scoped_to_the_calling_tenant` and
    ///   `tenant_isolation_for_read_and_delete` (REST).
    ///
    /// # Errors
    ///
    /// Returns [`DomainError::NotFound`] for foreign or missing ids.
    pub async fn delete_upstream(&self, tenant_id: Uuid, id: Uuid) -> Result<(), DomainError> {
        self.require_upstream(tenant_id, id).await?;
        let routes = self.store.list_routes_by_upstream(tenant_id, id).await?;
        for route in routes {
            self.store.delete_route(tenant_id, route.id).await?;
        }
        self.store.delete_upstream(tenant_id, id).await?;
        Ok(())
    }

    /// Lists the routes bound to one upstream.
    ///
    /// # Errors
    ///
    /// Returns [`DomainError::NotFound`] for foreign or missing upstreams.
    pub async fn list_routes_by_upstream(
        &self,
        tenant_id: Uuid,
        upstream_id: Uuid,
    ) -> Result<Vec<Route>, DomainError> {
        self.require_upstream(tenant_id, upstream_id).await?;
        self.store
            .list_routes_by_upstream(tenant_id, upstream_id)
            .await
    }

    // ------------------------------------------------------------------
    // Routes
    // ------------------------------------------------------------------

    /// Creates a route under an upstream owned by the calling tenant.
    ///
    /// # Errors
    ///
    /// Returns [`DomainError::NotFound`] when the upstream is not addressable,
    /// [`DomainError::ValidationError`] for mismatched protocols and
    /// [`DomainError::Conflict`] when the match rule is already taken.
    pub async fn create_route(
        &self,
        tenant_id: Uuid,
        draft: RouteDraft,
    ) -> Result<Route, DomainError> {
        let upstream = self.require_upstream(tenant_id, draft.upstream_id).await?;
        Self::validate_route_shape(&draft.match_config, draft.rate_limit.as_ref(), &draft.tags)?;
        Self::validate_route_protocol(&upstream, &draft.match_config)?;

        let siblings = self
            .store
            .list_routes_by_upstream(tenant_id, draft.upstream_id)
            .await?;
        let clash = siblings
            .iter()
            .find(|route| routes_overlap(route, draft.priority, &draft.match_config));
        if let Some(_route) = clash {
            return Err(DomainError::Conflict {
                detail: format!(
                    "a route of this upstream already matches {} at priority {}",
                    match_description(&draft.match_config),
                    draft.priority
                ),
                invalid_value: Some(match_description(&draft.match_config)),
            });
        }

        let now = now_millis();
        let route = Route {
            id: Uuid::new_v4(),
            tenant_id,
            upstream_id: draft.upstream_id,
            enabled: draft.enabled,
            priority: draft.priority,
            match_config: draft.match_config,
            plugins: draft.plugins,
            rate_limit: draft.rate_limit,
            tags: draft.tags,
            created_at: now,
            updated_at: now,
        };
        self.store.insert_route(route.clone()).await?;
        Ok(route)
    }

    /// Loads a single route.
    ///
    /// # Errors
    ///
    /// Returns [`DomainError::NotFound`] for foreign or missing ids.
    pub async fn get_route(&self, tenant_id: Uuid, id: Uuid) -> Result<Route, DomainError> {
        self.require_route(tenant_id, id).await
    }

    /// Lists the routes of a tenant.
    ///
    /// # Errors
    ///
    /// Returns a [`DomainError`] on storage failure.
    pub async fn list_routes(&self, tenant_id: Uuid) -> Result<Vec<Route>, DomainError> {
        self.store.list_routes(tenant_id).await
    }

    /// Replaces a route. `upstream_id` is immutable.
    ///
    /// # Errors
    ///
    /// Returns [`DomainError::NotFound`] for foreign or missing ids and
    /// [`DomainError::Conflict`] when the new match rule collides.
    pub async fn replace_route(
        &self,
        tenant_id: Uuid,
        id: Uuid,
        update: RouteUpdate,
    ) -> Result<Route, DomainError> {
        let existing = self.require_route(tenant_id, id).await?;
        Self::validate_route_shape(&update.match_config, update.rate_limit.as_ref(), &update.tags)?;

        let siblings = self
            .store
            .list_routes_by_upstream(tenant_id, existing.upstream_id)
            .await?;
        let clash = siblings
            .iter()
            .filter(|route| route.id != existing.id)
            .find(|route| routes_overlap(route, update.priority, &update.match_config));
        if let Some(_route) = clash {
            return Err(DomainError::Conflict {
                detail: format!(
                    "a route of this upstream already matches {} at priority {}",
                    match_description(&update.match_config),
                    update.priority
                ),
                invalid_value: Some(match_description(&update.match_config)),
            });
        }

        let mut updated = existing.clone();
        updated.enabled = update.enabled;
        updated.priority = update.priority;
        updated.match_config = update.match_config;
        updated.plugins = update.plugins;
        updated.rate_limit = update.rate_limit;
        updated.tags = update.tags;
        updated.updated_at = now_millis();
        self.store.update_route(updated.clone()).await?;
        Ok(updated)
    }

    /// Deletes a route.
    ///
    /// # Errors
    ///
    /// Returns [`DomainError::NotFound`] for foreign or missing ids.
    pub async fn delete_route(&self, tenant_id: Uuid, id: Uuid) -> Result<(), DomainError> {
        self.require_route(tenant_id, id).await?;
        self.store.delete_route(tenant_id, id).await?;
        Ok(())
    }

    // ------------------------------------------------------------------
    // Plugins
    // ------------------------------------------------------------------

    /// Creates an immutable custom plugin.
    ///
    /// # Errors
    ///
    /// Returns [`DomainError::ValidationError`] for empty sources and
    /// [`DomainError::Conflict`] when the name is already taken.
    pub async fn create_plugin(
        &self,
        tenant_id: Uuid,
        draft: PluginDraft,
    ) -> Result<Plugin, DomainError> {
        if draft.source_code.trim().is_empty() {
            return Err(DomainError::validation_with_value(
                "plugin source_code must not be empty",
                draft.name.clone(),
            ));
        }
        if draft.name.trim().is_empty() {
            return Err(DomainError::validation("plugin name must not be empty"));
        }
        if let Some(existing) = self
            .store
            .find_plugin_by_name(tenant_id, &draft.name)
            .await?
        {
            return Err(DomainError::Conflict {
                detail: format!("a plugin named '{}' already exists", draft.name),
                invalid_value: Some(existing.name),
            });
        }

        let now = now_millis();
        let plugin = Plugin {
            id: Uuid::new_v4(),
            tenant_id,
            plugin_type: draft.plugin_type,
            name: draft.name,
            description: draft.description,
            phases: draft.phases,
            config_schema: draft.config_schema,
            source_code: draft.source_code,
            created_at: now,
            updated_at: now,
            last_used_at: None,
            gc_eligible_at: Some(now + self.plugin_gc_ttl_secs * 1_000),
        };
        self.store.insert_plugin(plugin.clone()).await?;
        Ok(plugin)
    }

    /// Loads a single plugin.
    ///
    /// # Errors
    ///
    /// Returns [`DomainError::NotFound`] for foreign or missing ids.
    pub async fn get_plugin(&self, tenant_id: Uuid, id: Uuid) -> Result<Plugin, DomainError> {
        self.require_plugin(tenant_id, id).await
    }

    /// Lists the plugins of a tenant.
    ///
    /// # Errors
    ///
    /// Returns a [`DomainError`] on storage failure.
    pub async fn list_plugins(&self, tenant_id: Uuid) -> Result<Vec<Plugin>, DomainError> {
        self.store.list_plugins(tenant_id).await
    }

    /// Returns the Starlark source of a plugin (`GET /plugins/{id}/source`).
    ///
    /// # Errors
    ///
    /// Returns [`DomainError::NotFound`] for foreign or missing ids.
    pub async fn get_plugin_source(
        &self,
        tenant_id: Uuid,
        id: Uuid,
    ) -> Result<(Plugin, String), DomainError> {
        let plugin = self.require_plugin(tenant_id, id).await?;
        let source_code = plugin.source_code.clone();
        Ok((plugin, source_code))
    }

    /// Deletes a plugin unless it is still referenced.
    ///
    /// # Errors
    ///
    /// Returns [`DomainError::NotFound`] for foreign or missing ids and
    /// [`DomainError::PluginInUse`] when bindings exist (ADR 0001).
    pub async fn delete_plugin(&self, tenant_id: Uuid, id: Uuid) -> Result<(), DomainError> {
        let plugin = self.require_plugin(tenant_id, id).await?;
        let referenced_by = self.plugin_references(tenant_id, &plugin).await?;
        if !referenced_by.is_empty() {
            return Err(DomainError::PluginInUse {
                plugin_id: plugin.gts_id(),
                referenced_by,
            });
        }
        self.store.delete_plugin(tenant_id, id).await?;
        Ok(())
    }

    /// Scans upstream and route bindings for references to `plugin_id`.
    async fn plugin_references(
        &self,
        tenant_id: Uuid,
        plugin: &Plugin,
    ) -> Result<ReferencedBy, DomainError> {
        let custom_ref = crate::domain::models::gts_instance_id(
            plugin.plugin_type.type_id(),
            plugin.id,
        );
        let mut referenced_by = ReferencedBy::empty();
        for upstream in self.store.list_upstreams(tenant_id).await? {
            if references_plugin(&upstream_references(&upstream), plugin.id, &custom_ref) {
                referenced_by
                    .upstreams
                    .push(crate::domain::models::gts_instance_id(
                        UPSTREAM_TYPE,
                        upstream.id,
                    ));
            }
        }
        for route in self.store.list_routes(tenant_id).await? {
            if references_plugin(&route_references(&route), plugin.id, &custom_ref) {
                referenced_by
                    .routes
                    .push(crate::domain::models::gts_instance_id(
                        crate::domain::models::ROUTE_TYPE,
                        route.id,
                    ));
            }
        }
        Ok(referenced_by)
    }

    // ------------------------------------------------------------------
    // helpers
    // ------------------------------------------------------------------

    async fn require_upstream(&self, tenant_id: Uuid, id: Uuid) -> Result<Upstream, DomainError> {
        self.store
            .get_upstream(tenant_id, id)
            .await?
            .ok_or_else(|| DomainError::not_found(ResourceKind::Upstream, &id.to_string()))
    }

    async fn require_route(&self, tenant_id: Uuid, id: Uuid) -> Result<Route, DomainError> {
        self.store
            .get_route(tenant_id, id)
            .await?
            .ok_or_else(|| DomainError::not_found(ResourceKind::Route, &id.to_string()))
    }

    async fn require_plugin(&self, tenant_id: Uuid, id: Uuid) -> Result<Plugin, DomainError> {
        self.store
            .get_plugin(tenant_id, id)
            .await?
            .ok_or_else(|| DomainError::not_found(ResourceKind::Plugin, &id.to_string()))
    }

    fn resolve_new_alias(
        requested: Option<&str>,
        endpoints: &[Endpoint],
    ) -> Result<String, DomainError> {
        let derivation = crate::domain::alias::derive_alias(endpoints);
        let explicit = requested.map(normalize_alias);
        if let Some(alias) = &explicit
            && !alias_is_valid(alias)
        {
            return Err(DomainError::validation_with_value(
                "alias does not match the required pattern",
                alias.clone(),
            ));
        }
        match (explicit, derivation) {
            (Some(alias), Ok(derived)) => {
                if alias != derived.alias {
                    return Err(DomainError::AliasMismatch {
                        detail: format!(
                            "alias '{alias}' does not match the alias derived from the endpoint \
                             pool '{}'",
                            derived.alias
                        ),
                        provided: alias,
                        derived: derived.alias,
                    });
                }
                Ok(alias)
            }
            (Some(alias), Err(_)) => Ok(alias),
            (None, Ok(derived)) => Ok(derived.alias),
            (None, Err(err)) => Err(err.into_domain()),
        }
    }

    fn validate_upstream_shape(
        allow_http_upstream: bool,
        server: &ServerConfig,
        auth: Option<&crate::domain::models::AuthConfig>,
        plugins: Option<&crate::domain::models::PluginsConfig>,
        rate_limit: Option<&RateLimitConfig>,
        cors: Option<&CorsConfig>,
        tags: &[String],
    ) -> Result<(), DomainError> {
        crate::domain::models::validate_endpoint_pool(&server.endpoints, allow_http_upstream)?;

        if let Some(auth) = auth
            && let Some(plugin_type) = &auth.plugin_type
        {
            if plugin_type.trim().is_empty() {
                return Err(DomainError::validation_with_value(
                    "auth.plugin_type must not be empty",
                    plugin_type.clone(),
                ));
            }
            if !builtin_plugin_is_bindable(plugin_type) {
                return Err(DomainError::validation_with_value(
                    "unknown auth plugin: the identifier exists in the types-registry but has no \
                     bindable implementation",
                    plugin_type.clone(),
                ));
            }
        }

        if let Some(plugins) = plugins {
            for reference in &plugins.items {
                if reference.trim().is_empty() {
                    return Err(DomainError::validation(
                        "plugins.items entries must not be empty",
                    ));
                }
                if !builtin_plugin_is_bindable(reference)
                    && crate::domain::models::strip_gts_prefix(
                        crate::domain::models::GUARD_PLUGIN_TYPE,
                        reference,
                    )
                    .is_none()
                    && crate::domain::models::strip_gts_prefix(
                        crate::domain::models::TRANSFORM_PLUGIN_TYPE,
                        reference,
                    )
                    .is_none()
                    && crate::domain::models::strip_gts_prefix(
                        crate::domain::models::AUTH_PLUGIN_TYPE,
                        reference,
                    )
                    .is_none()
                {
                    return Err(DomainError::validation_with_value(
                        "plugins.items entries must be a built-in plugin or a custom plugin \
                         identifier",
                        reference.clone(),
                    ));
                }
            }
        }

        if let Some(rate_limit) = rate_limit {
            if rate_limit.sustained.rate == 0 {
                return Err(DomainError::validation_with_value(
                    "rate_limit.sustained.rate must be at least 1",
                    "0".to_owned(),
                ));
            }
            // The compiled parameters are executed by the data plane; the
            // control plane still checks that the derived bucket is usable so
            // an unusable configuration is rejected at write time.
            let parameters =
                crate::domain::rate_limit::RateLimitParameters::from_config(rate_limit);
            if parameters.capacity == 0 {
                return Err(DomainError::validation_with_value(
                    "rate_limit.burst.capacity must be at least 1",
                    "0".to_owned(),
                ));
            }
            if parameters.cost > parameters.capacity {
                return Err(DomainError::validation_with_value(
                    "rate_limit.cost must not exceed the bucket capacity",
                    parameters.cost.to_string(),
                ));
            }
        }

        if let Some(cors) = cors {
            if cors.allow_credentials && cors.allowed_origins.iter().any(|origin| origin == "*") {
                return Err(DomainError::validation_with_value(
                    "cors.allow_credentials cannot be combined with the wildcard origin",
                    "*".to_owned(),
                ));
            }
            if cors.enabled && cors.allowed_origins.is_empty() {
                return Err(DomainError::validation(
                    "cors.enabled requires at least one allowed origin",
                ));
            }
        }

        for tag in tags {
            if !crate::domain::models::tag_is_valid(tag) {
                return Err(DomainError::validation_with_value(
                    "tags must match ^[a-z0-9_-]+$",
                    tag.clone(),
                ));
            }
        }

        Ok(())
    }

    fn validate_route_shape(
        match_config: &MatchConfig,
        rate_limit: Option<&RateLimitConfig>,
        tags: &[String],
    ) -> Result<(), DomainError> {
        match (&match_config.http, &match_config.grpc) {
            (Some(_), None) | (None, Some(_)) => {}
            _ => {
                return Err(DomainError::validation(
                    "route.match must set exactly one of `http` or `grpc`",
                ));
            }
        }
        if let Some(http) = &match_config.http {
            if http.methods.is_empty() {
                return Err(DomainError::validation(
                    "route.match.http.methods must contain at least one method",
                ));
            }
            if http.path.trim().is_empty() {
                return Err(DomainError::validation(
                    "route.match.http.path must not be empty",
                ));
            }
            if !http.path.starts_with('/') {
                return Err(DomainError::validation_with_value(
                    "route.match.http.path must be an absolute path",
                    http.path.clone(),
                ));
            }
        }
        if let Some(grpc) = &match_config.grpc {
            let GrpcMatch { service, method } = grpc;
            if service.trim().is_empty() || method.trim().is_empty() {
                return Err(DomainError::validation(
                    "route.match.grpc requires a service and a method",
                ));
            }
        }
        if let Some(rate_limit) = rate_limit
            && rate_limit.sustained.rate == 0
        {
            return Err(DomainError::validation_with_value(
                "rate_limit.sustained.rate must be at least 1",
                "0".to_owned(),
            ));
        }
        for tag in tags {
            if !crate::domain::models::tag_is_valid(tag) {
                return Err(DomainError::validation_with_value(
                    "tags must match ^[a-z0-9_-]+$",
                    tag.clone(),
                ));
            }
        }
        Ok(())
    }

    fn validate_route_protocol(
        upstream: &Upstream,
        match_config: &MatchConfig,
    ) -> Result<(), DomainError> {
        let is_grpc = upstream.protocol == crate::domain::models::Protocol::Grpc;
        let matches = match match_config.kind() {
            crate::domain::models::MatchKind::Http => !is_grpc,
            crate::domain::models::MatchKind::Grpc => is_grpc,
        };
        if !matches {
            return Err(DomainError::validation_with_value(
                "route.match kind must match the upstream protocol",
                format!("{:?}", match_config.kind()),
            ));
        }
        Ok(())
    }
}

/// Every plugin reference a upstream carries.
fn upstream_references(upstream: &Upstream) -> Vec<&str> {
    let mut refs: Vec<&str> = Vec::new();
    if let Some(auth) = &upstream.auth
        && let Some(plugin_type) = &auth.plugin_type
    {
        refs.push(plugin_type.as_str());
    }
    if let Some(plugins) = &upstream.plugins {
        refs.extend(plugins.items.iter().map(String::as_str));
    }
    refs
}

/// Every plugin reference a route carries.
fn route_references(route: &Route) -> Vec<&str> {
    match &route.plugins {
        Some(plugins) => plugins.items.iter().map(String::as_str).collect(),
        None => Vec::new(),
    }
}

/// Whether one of `references` addresses the plugin.
///
/// A custom plugin may be bound either by its anonymous GTS id or by its bare
/// UUID, so both spellings resolve to the instance UUID before comparing.
fn references_plugin(references: &[&str], plugin_id: Uuid, custom_ref: &str) -> bool {
    references.iter().any(|reference| {
        *reference == custom_ref
            || crate::domain::models::strip_gts_prefix_of_any(reference)
                .unwrap_or_else(|| (*reference).to_owned())
                == plugin_id.to_string()
    })
}

/// Whether `existing` already claims the same match rule as `candidate`.
fn routes_overlap(existing: &Route, priority: i32, candidate: &MatchConfig) -> bool {
    if existing.priority != priority {
        return false;
    }
    match (&existing.match_config.http, &candidate.http) {
        (Some(a), Some(b)) => {
            a.path == b.path && a.methods.iter().any(|method| b.methods.contains(method))
        }
        _ => match (&existing.match_config.grpc, &candidate.grpc) {
            (Some(a), Some(b)) => a.service == b.service && a.method == b.method,
            _ => false,
        },
    }
}

fn match_description(match_config: &MatchConfig) -> String {
    match (&match_config.http, &match_config.grpc) {
        (Some(http), _) => {
            let methods = http
                .methods
                .iter()
                .map(|method| format!("{method:?}"))
                .collect::<Vec<_>>()
                .join(",");
            format!("{} {methods}", http.path)
        }
        (None, Some(grpc)) => format!("{}/{}", grpc.service, grpc.method),
        (None, None) => "unspecified match".to_owned(),
    }
}

#[cfg(test)]
#[path = "service_tests.rs"]
mod tests;
