//! Control Plane: owns configuration data (upstreams, routes, plugins),
//! alias resolution and effective-config assembly (ADR-0001).

use std::sync::Arc;

use serde_json::Value;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::domain::alias;
use crate::domain::dto::{
    EffectiveConfig, PluginWriteInput, RouteWriteInput, UpstreamWriteInput,
};
use crate::domain::error::{ErrorKind, OagwError, OagwResult};
use crate::domain::gts_helpers;
use crate::domain::model::{
    Endpoint, HttpMatch, MatchConfig, Plugin, PluginKind, PluginsConfig, Route, ServerConfig,
    Upstream,
};
use crate::domain::plugin::PluginCatalog;
use crate::domain::repo::{PluginRepository, RouteRepository, TenantDirectory, UpstreamRepository};
use crate::domain::services::merge::{self, Layer};
use crate::util::now_rfc3339;

/// An alias resolved across the tenant hierarchy.
#[derive(Debug, Clone)]
pub struct ResolvedAlias {
    /// The routing target: closest tenant that declares the alias.
    pub selected: Upstream,
    /// Every upstream declaring this alias, ordered **root first**.
    pub chain_root_first: Vec<Upstream>,
    /// `false` when any tenant in the chain disabled the upstream — a
    /// descendant cannot re-enable an ancestor-disabled resource.
    pub enabled: bool,
    /// Tenant that issued the proxy request.
    pub caller_tenant_id: Uuid,
}

/// A matched proxy target: upstream, route and the merged configuration.
#[derive(Debug, Clone)]
pub struct ProxyTarget {
    pub upstream: Upstream,
    pub route: Route,
    pub effective: EffectiveConfig,
    /// Outbound path (`match.http.path` + the accepted path suffix).
    pub outbound_path: String,
}

pub struct ControlPlaneService {
    upstreams: Arc<dyn UpstreamRepository>,
    routes: Arc<dyn RouteRepository>,
    plugins: Arc<dyn PluginRepository>,
    directory: Arc<dyn TenantDirectory>,
    catalog: Arc<dyn PluginCatalog>,
}

impl ControlPlaneService {
    #[must_use]
    pub fn new(
        upstreams: Arc<dyn UpstreamRepository>,
        routes: Arc<dyn RouteRepository>,
        plugins: Arc<dyn PluginRepository>,
        directory: Arc<dyn TenantDirectory>,
        catalog: Arc<dyn PluginCatalog>,
    ) -> Self {
        Self {
            upstreams,
            routes,
            plugins,
            directory,
            catalog,
        }
    }

    // -----------------------------------------------------------------
    // Upstream CRUD
    // -----------------------------------------------------------------

    /// # Errors
    /// `400` on invalid configuration, `409` when `(tenant_id, alias)` exists.
    pub async fn create_upstream(
        &self,
        ctx: &SecurityContext,
        input: UpstreamWriteInput,
    ) -> OagwResult<Upstream> {
        let tenant_id = ctx.subject_tenant_id();
        let protocol = validate_protocol(&input.protocol)?;
        let endpoints = build_endpoints(&input)?;
        alias::validate_endpoints(&endpoints)?;
        let alias_value = alias::resolve_create_alias(&endpoints, input.alias.as_deref())?;

        if self
            .upstreams
            .find_by_alias(tenant_id, &alias_value)
            .await?
            .is_some()
        {
            return Err(
                OagwError::new(ErrorKind::Conflict, format!(
                    "an upstream with alias '{alias_value}' already exists for this tenant"
                ))
                .with_ext("alias", alias_value),
            );
        }

        let plugins = input.plugins.unwrap_or_default();
        self.validate_auth(input.auth.as_ref(), tenant_id).await?;
        self.validate_plugin_chain(&plugins, tenant_id).await?;
        validate_cors(input.cors.as_ref())?;
        let tags = validate_tags(input.tags.unwrap_or_default())?;

        let now = now_rfc3339();
        let upstream = Upstream {
            id: Uuid::new_v4(),
            tenant_id,
            alias: alias_value,
            protocol,
            enabled: input.enabled.unwrap_or(true),
            server: ServerConfig { endpoints },
            auth: input.auth,
            headers: input.headers.unwrap_or_default(),
            rate_limit: input.rate_limit,
            cors: input.cors,
            plugins,
            tags,
            created_at: now.clone(),
            updated_at: now,
        };
        audit_config_change("upstream_created", tenant_id, upstream.id, Some(&upstream.alias));
        self.upstreams.insert(upstream).await
    }

    /// # Errors
    /// `404` when the upstream is not owned by the calling tenant.
    pub async fn get_upstream(&self, ctx: &SecurityContext, id: Uuid) -> OagwResult<Upstream> {
        self.upstreams
            .get(ctx.subject_tenant_id(), id)
            .await?
            .ok_or_else(|| OagwError::not_found(format!("upstream '{id}' not found")))
    }

    /// # Errors
    /// Propagates repository failures.
    pub async fn list_upstreams(&self, ctx: &SecurityContext) -> OagwResult<Vec<Upstream>> {
        self.upstreams.list(ctx.subject_tenant_id()).await
    }

    /// Full replacement; omitted optional members are cleared.
    ///
    /// # Errors
    /// `404` when absent, `400` when the change would move the alias.
    pub async fn replace_upstream(
        &self,
        ctx: &SecurityContext,
        id: Uuid,
        input: UpstreamWriteInput,
    ) -> OagwResult<Upstream> {
        let tenant_id = ctx.subject_tenant_id();
        let existing = self.get_upstream(ctx, id).await?;
        let protocol = validate_protocol(&input.protocol)?;
        let endpoints = build_endpoints(&input)?;
        alias::validate_endpoints(&endpoints)?;
        let alias_value =
            alias::enforce_alias_update(&existing.alias, &endpoints, input.alias.as_deref())?;

        let plugins = input.plugins.unwrap_or_default();
        self.validate_auth(input.auth.as_ref(), tenant_id).await?;
        self.validate_plugin_chain(&plugins, tenant_id).await?;
        validate_cors(input.cors.as_ref())?;
        let tags = validate_tags(input.tags.unwrap_or_default())?;

        let upstream = Upstream {
            id: existing.id,
            tenant_id,
            alias: alias_value,
            protocol,
            enabled: input.enabled.unwrap_or(true),
            server: ServerConfig { endpoints },
            auth: input.auth,
            headers: input.headers.unwrap_or_default(),
            rate_limit: input.rate_limit,
            cors: input.cors,
            plugins,
            tags,
            created_at: existing.created_at,
            updated_at: now_rfc3339(),
        };
        audit_config_change("upstream_replaced", tenant_id, upstream.id, Some(&upstream.alias));
        self.upstreams.replace(upstream).await
    }

    /// # Errors
    /// `404` when the upstream is not owned by the calling tenant.
    pub async fn delete_upstream(&self, ctx: &SecurityContext, id: Uuid) -> OagwResult<()> {
        let tenant_id = ctx.subject_tenant_id();
        if !self.upstreams.delete(tenant_id, id).await? {
            return Err(OagwError::not_found(format!("upstream '{id}' not found")));
        }
        // FK `oagw_route.upstream_id` is declared ON DELETE CASCADE.
        self.routes.delete_by_upstream(id).await?;
        audit_config_change("upstream_deleted", tenant_id, id, None);
        Ok(())
    }

    // -----------------------------------------------------------------
    // Route CRUD
    // -----------------------------------------------------------------

    /// # Errors
    /// `400` on an unknown upstream or malformed match, `409` on a duplicate
    /// match rule.
    pub async fn create_route(
        &self,
        ctx: &SecurityContext,
        input: RouteWriteInput,
    ) -> OagwResult<Route> {
        let tenant_id = ctx.subject_tenant_id();
        let raw_upstream = input.upstream_id.as_deref().ok_or_else(|| {
            OagwError::validation("upstream_id is required")
        })?;
        let upstream_id = gts_helpers::parse_resource_id(raw_upstream, gts_helpers::UPSTREAM_TYPE)
            .ok_or_else(|| {
                OagwError::validation(format!("upstream_id '{raw_upstream}' is not a valid id"))
            })?;

        // Ancestor upstreams are not directly addressable from the management API.
        let upstream = self
            .upstreams
            .get(tenant_id, upstream_id)
            .await?
            .ok_or_else(|| {
                OagwError::validation(format!("upstream '{upstream_id}' not found for this tenant"))
            })?;

        let match_config = validate_match(&input.match_config, &upstream)?;
        let plugins = input.plugins.unwrap_or_default();
        self.validate_plugin_chain(&plugins, tenant_id).await?;
        validate_cors(input.cors.as_ref())?;
        let tags = validate_tags(input.tags.unwrap_or_default())?;
        let priority = input.priority.unwrap_or(0);

        let siblings = self.routes.list_by_upstream(upstream_id).await?;
        ensure_match_unique(&siblings, None, &match_config, priority)?;

        let now = now_rfc3339();
        let route = Route {
            id: Uuid::new_v4(),
            tenant_id,
            upstream_id,
            match_type: match_config.match_type().to_owned(),
            priority,
            enabled: input.enabled.unwrap_or(true),
            match_config,
            rate_limit: input.rate_limit,
            cors: input.cors,
            plugins,
            tags,
            created_at: now.clone(),
            updated_at: now,
        };
        audit_config_change("route_created", tenant_id, route.id, None);
        self.routes.insert(route).await
    }

    /// # Errors
    /// `404` when the route is not owned by the calling tenant.
    pub async fn get_route(&self, ctx: &SecurityContext, id: Uuid) -> OagwResult<Route> {
        self.routes
            .get(ctx.subject_tenant_id(), id)
            .await?
            .ok_or_else(|| OagwError::not_found(format!("route '{id}' not found")))
    }

    /// # Errors
    /// Propagates repository failures.
    pub async fn list_routes(&self, ctx: &SecurityContext) -> OagwResult<Vec<Route>> {
        self.routes.list(ctx.subject_tenant_id()).await
    }

    /// # Errors
    /// `404` when absent, `409` on a duplicate match rule.
    pub async fn replace_route(
        &self,
        ctx: &SecurityContext,
        id: Uuid,
        input: RouteWriteInput,
    ) -> OagwResult<Route> {
        let tenant_id = ctx.subject_tenant_id();
        let existing = self.get_route(ctx, id).await?;

        // `upstream_id` is immutable; a differing value in the body is rejected
        // rather than silently ignored.
        if let Some(raw) = input.upstream_id.as_deref() {
            let requested =
                gts_helpers::parse_resource_id(raw, gts_helpers::UPSTREAM_TYPE).ok_or_else(|| {
                    OagwError::validation(format!("upstream_id '{raw}' is not a valid id"))
                })?;
            if requested != existing.upstream_id {
                return Err(OagwError::validation(
                    "upstream_id is immutable and cannot be changed on replace",
                ));
            }
        }

        let upstream = self
            .upstreams
            .get(tenant_id, existing.upstream_id)
            .await?
            .ok_or_else(|| {
                OagwError::validation(format!(
                    "upstream '{}' not found for this tenant",
                    existing.upstream_id
                ))
            })?;

        let match_config = validate_match(&input.match_config, &upstream)?;
        let plugins = input.plugins.unwrap_or_default();
        self.validate_plugin_chain(&plugins, tenant_id).await?;
        validate_cors(input.cors.as_ref())?;
        let tags = validate_tags(input.tags.unwrap_or_default())?;
        let priority = input.priority.unwrap_or(0);

        let siblings = self.routes.list_by_upstream(existing.upstream_id).await?;
        ensure_match_unique(&siblings, Some(existing.id), &match_config, priority)?;

        let route = Route {
            id: existing.id,
            tenant_id,
            upstream_id: existing.upstream_id,
            match_type: match_config.match_type().to_owned(),
            priority,
            enabled: input.enabled.unwrap_or(true),
            match_config,
            rate_limit: input.rate_limit,
            cors: input.cors,
            plugins,
            tags,
            created_at: existing.created_at,
            updated_at: now_rfc3339(),
        };
        audit_config_change("route_replaced", tenant_id, route.id, None);
        self.routes.replace(route).await
    }

    /// # Errors
    /// `404` when the route is not owned by the calling tenant.
    pub async fn delete_route(&self, ctx: &SecurityContext, id: Uuid) -> OagwResult<()> {
        if !self.routes.delete(ctx.subject_tenant_id(), id).await? {
            return Err(OagwError::not_found(format!("route '{id}' not found")));
        }
        audit_config_change("route_deleted", ctx.subject_tenant_id(), id, None);
        Ok(())
    }

    // -----------------------------------------------------------------
    // Plugin CRUD (custom, UUID-backed)
    // -----------------------------------------------------------------

    /// # Errors
    /// `400` on invalid input, `409` when the name is taken.
    pub async fn create_plugin(
        &self,
        ctx: &SecurityContext,
        input: PluginWriteInput,
    ) -> OagwResult<Plugin> {
        let tenant_id = ctx.subject_tenant_id();
        if input.name.trim().is_empty() {
            return Err(OagwError::validation("plugin name must not be empty"));
        }
        if input.source_code.trim().is_empty() {
            return Err(OagwError::validation("source_code must not be empty"));
        }
        if self
            .plugins
            .find_by_name(tenant_id, &input.name)
            .await?
            .is_some()
        {
            return Err(OagwError::new(
                ErrorKind::Conflict,
                format!("a plugin named '{}' already exists for this tenant", input.name),
            ));
        }

        let phases = input.phases.unwrap_or_else(|| match input.plugin_type {
            PluginKind::Auth => vec!["on_request".to_owned()],
            PluginKind::Guard => vec!["on_request".to_owned()],
            PluginKind::Transform => {
                vec!["on_request".to_owned(), "on_response".to_owned()]
            }
        });

        let plugin = Plugin {
            id: Uuid::new_v4(),
            tenant_id,
            plugin_type: input.plugin_type,
            name: input.name,
            description: input.description,
            phases,
            config_schema: input.config_schema.unwrap_or_default(),
            source_code: input.source_code,
            created_at: now_rfc3339(),
            last_used_at: None,
            gc_eligible_at: None,
        };
        self.plugins.insert(plugin).await
    }

    /// # Errors
    /// `404` when the plugin is not owned by the calling tenant.
    pub async fn get_plugin(&self, ctx: &SecurityContext, id: Uuid) -> OagwResult<Plugin> {
        self.plugins
            .get(ctx.subject_tenant_id(), id)
            .await?
            .ok_or_else(|| OagwError::not_found(format!("plugin '{id}' not found")))
    }

    /// # Errors
    /// Propagates repository failures.
    pub async fn list_plugins(&self, ctx: &SecurityContext) -> OagwResult<Vec<Plugin>> {
        self.plugins.list(ctx.subject_tenant_id()).await
    }

    /// # Errors
    /// `404` when absent, `409 PluginInUse` when still referenced.
    pub async fn delete_plugin(&self, ctx: &SecurityContext, id: Uuid) -> OagwResult<()> {
        let plugin = self.get_plugin(ctx, id).await?;
        let (upstreams, routes) = self.plugin_references(id).await?;
        if !upstreams.is_empty() || !routes.is_empty() {
            let referenced_by = serde_json::json!({
                "upstreams": upstreams,
                "routes": routes,
            });
            return Err(OagwError::new(
                ErrorKind::PluginInUse,
                format!(
                    "Plugin is referenced by {} upstream(s) and {} route(s)",
                    upstreams.len(),
                    routes.len()
                ),
            )
            .with_ext("plugin_id", plugin.gts_id())
            .with_ext("referenced_by", referenced_by));
        }
        self.plugins.delete(ctx.subject_tenant_id(), id).await?;
        Ok(())
    }

    /// Anonymous GTS ids of the upstreams and routes referencing `plugin_id`.
    ///
    /// # Errors
    /// Propagates repository failures.
    pub async fn plugin_references(
        &self,
        plugin_id: Uuid,
    ) -> OagwResult<(Vec<String>, Vec<String>)> {
        let mut upstream_refs = Vec::new();
        for upstream in self.upstreams.all().await? {
            let in_auth = upstream
                .auth
                .as_ref()
                .and_then(|a| a.plugin_type.as_deref())
                .and_then(gts_helpers::plugin_ref_uuid)
                == Some(plugin_id);
            let in_chain = upstream
                .plugins
                .items
                .iter()
                .any(|b| b.plugin_uuid == Some(plugin_id));
            if in_auth || in_chain {
                upstream_refs.push(gts_helpers::anonymous_id(
                    gts_helpers::UPSTREAM_TYPE,
                    upstream.id,
                ));
            }
        }
        let mut route_refs = Vec::new();
        for route in self.routes.all().await? {
            if route
                .plugins
                .items
                .iter()
                .any(|b| b.plugin_uuid == Some(plugin_id))
            {
                route_refs.push(gts_helpers::anonymous_id(gts_helpers::ROUTE_TYPE, route.id));
            }
        }
        Ok((upstream_refs, route_refs))
    }

    /// Mark newly-unlinked plugins GC-eligible and delete expired ones.
    ///
    /// # Errors
    /// Propagates repository failures.
    pub async fn run_plugin_gc(&self, ttl_days: u64) -> OagwResult<usize> {
        let now = crate::util::unix_now();
        let ttl_secs = ttl_days.saturating_mul(86_400);
        let mut deleted = 0usize;
        for plugin in self.plugins.all().await? {
            let (up, rt) = self.plugin_references(plugin.id).await?;
            let linked = !up.is_empty() || !rt.is_empty();
            match (linked, plugin.gc_eligible_at.as_deref()) {
                (true, Some(_)) => self.plugins.set_gc_eligible_at(plugin.id, None).await?,
                (false, None) => {
                    let at = crate::util::format_rfc3339(now.saturating_add(ttl_secs));
                    self.plugins.set_gc_eligible_at(plugin.id, Some(at)).await?;
                }
                (false, Some(at)) => {
                    if at.as_bytes() <= crate::util::format_rfc3339(now).as_bytes() {
                        self.plugins.delete(plugin.tenant_id, plugin.id).await?;
                        deleted += 1;
                    }
                }
                (true, None) => {}
            }
        }
        Ok(deleted)
    }

    // -----------------------------------------------------------------
    // Proxy-time resolution
    // -----------------------------------------------------------------

    /// Walk the tenant chain descendant → root and return every upstream that
    /// declares `alias`; the closest one is the routing target.
    ///
    /// # Errors
    /// Propagates repository failures.
    pub async fn resolve_alias(
        &self,
        ctx: &SecurityContext,
        alias_value: &str,
    ) -> OagwResult<Option<ResolvedAlias>> {
        let normalized = alias::normalize_alias(alias_value);
        let chain = self.directory.chain(ctx, ctx.subject_tenant_id()).await;

        let mut descendant_first: Vec<Upstream> = Vec::new();
        for tenant_id in chain {
            if let Some(u) = self.upstreams.find_by_alias(tenant_id, &normalized).await? {
                descendant_first.push(u);
            }
        }
        let Some(selected) = descendant_first.first().cloned() else {
            return Ok(None);
        };
        let enabled = descendant_first.iter().all(|u| u.enabled);
        let mut chain_root_first = descendant_first;
        chain_root_first.reverse();
        Ok(Some(ResolvedAlias {
            selected,
            chain_root_first,
            enabled,
            caller_tenant_id: ctx.subject_tenant_id(),
        }))
    }

    /// Match a route for an already-resolved alias and assemble the effective
    /// configuration.
    ///
    /// `path_suffix` is the part of the proxy URL after `{alias}`, with its
    /// leading `/` (empty when absent).
    ///
    /// # Errors
    /// `404 RouteNotFound` when nothing matches, `400` when a path suffix is
    /// supplied to a route with `path_suffix_mode: disabled`.
    pub async fn match_route(
        &self,
        resolved: &ResolvedAlias,
        method: &str,
        path_suffix: &str,
    ) -> OagwResult<ProxyTarget> {
        // Descendant routes take priority: `chain_root_first` is reversed so
        // index 0 is the calling tenant's own upstream.
        let mut candidates: Vec<(usize, Route)> = Vec::new();
        for (distance, upstream) in resolved.chain_root_first.iter().rev().enumerate() {
            for route in self.routes.list_by_upstream(upstream.id).await? {
                if route.enabled {
                    candidates.push((distance, route));
                }
            }
        }

        let suffix = normalize_path(path_suffix);
        let mut best: Option<(usize, usize, i32, Route)> = None;
        for (distance, route) in candidates {
            let Some(http) = route.match_config.http.as_ref() else {
                continue;
            };
            if !http
                .methods
                .iter()
                .any(|m| m.eq_ignore_ascii_case(method))
            {
                continue;
            }
            let route_path = normalize_path(&http.path);
            if !path_matches(&suffix, &route_path) {
                continue;
            }
            let score = (distance, route_path.len(), route.priority);
            let better = best.as_ref().is_none_or(|(d, len, prio, _)| {
                (score.0, std::cmp::Reverse(score.1), std::cmp::Reverse(score.2))
                    < (*d, std::cmp::Reverse(*len), std::cmp::Reverse(*prio))
            });
            if better {
                best = Some((score.0, score.1, score.2, route));
            }
        }

        let (_, _, _, route) = best.ok_or_else(|| {
            OagwError::new(
                ErrorKind::RouteNotFound,
                format!("no route matches {method} {suffix} on alias '{}'", resolved.selected.alias),
            )
            .with_ext("alias", resolved.selected.alias.clone())
        })?;

        let http = route
            .match_config
            .http
            .as_ref()
            .ok_or_else(|| OagwError::internal("matched route lost its HTTP match keys"))?;
        let route_path = normalize_path(&http.path);
        let remainder = suffix
            .strip_prefix(route_path.as_str())
            .unwrap_or("")
            .to_owned();
        if !remainder.is_empty()
            && http.path_suffix_mode == crate::domain::model::PathSuffixMode::Disabled
        {
            return Err(OagwError::validation(format!(
                "route '{}' has path_suffix_mode: disabled but a path suffix was supplied",
                http.path
            )));
        }
        let outbound_path = format!("{route_path}{remainder}");

        // The matched route may belong to an ancestor; the *routing target*
        // stays the closest upstream, so endpoints and credentials come from
        // there while ancestor constraints still merge in below.
        let layers: Vec<Layer<'_>> = resolved
            .chain_root_first
            .iter()
            .map(|u| Layer {
                upstream: u,
                is_own: u.tenant_id == resolved.caller_tenant_id,
            })
            .collect();
        let effective = merge::effective_config(&layers, Some(&route));

        Ok(ProxyTarget {
            upstream: resolved.selected.clone(),
            route,
            effective,
            outbound_path,
        })
    }

    // -----------------------------------------------------------------
    // Validation helpers
    // -----------------------------------------------------------------

    async fn validate_auth(
        &self,
        auth: Option<&crate::domain::model::AuthConfig>,
        tenant_id: Uuid,
    ) -> OagwResult<()> {
        let Some(auth) = auth else { return Ok(()) };
        let Some(plugin_type) = auth.plugin_type.as_deref() else {
            return Ok(());
        };
        if plugin_type.trim().is_empty() {
            return Err(OagwError::validation("auth.type must not be empty"));
        }
        if let Some(uuid) = gts_helpers::plugin_ref_uuid(plugin_type) {
            let plugin = self.plugins.get(tenant_id, uuid).await?.ok_or_else(|| {
                OagwError::validation(format!("unknown auth plugin: {plugin_type}"))
            })?;
            if plugin.plugin_type != PluginKind::Auth {
                return Err(OagwError::validation(format!(
                    "plugin '{plugin_type}' is a {} plugin, not an auth plugin",
                    plugin.plugin_type.as_str()
                )));
            }
            return Ok(());
        }
        let (base, _) = gts_helpers::split_gts(plugin_type).ok_or_else(|| {
            OagwError::validation(format!("auth.type '{plugin_type}' is not a GTS identifier"))
        })?;
        if base != gts_helpers::AUTH_PLUGIN_TYPE {
            return Err(OagwError::validation(format!(
                "auth.type must be a '{}' identifier, got '{plugin_type}'",
                gts_helpers::AUTH_PLUGIN_TYPE
            )));
        }
        if !self.catalog.has_auth(plugin_type) {
            return Err(OagwError::validation(format!(
                "unknown auth plugin: {plugin_type}"
            )));
        }
        Ok(())
    }

    async fn validate_plugin_chain(
        &self,
        plugins: &PluginsConfig,
        tenant_id: Uuid,
    ) -> OagwResult<()> {
        for binding in &plugins.items {
            let plugin_ref = binding.plugin_ref.as_str();
            if let Some(uuid) = binding.plugin_uuid {
                let plugin = self.plugins.get(tenant_id, uuid).await?.ok_or_else(|| {
                    OagwError::validation(format!("unknown plugin: {plugin_ref}"))
                })?;
                // When the binding spells the full GTS id, the base type has
                // to agree with the stored plugin kind.
                if let Some((base, _)) = gts_helpers::split_gts(plugin_ref)
                    && plugin_ref != uuid.to_string()
                    && PluginKind::from_base_type(base) != Some(plugin.plugin_type)
                {
                    return Err(OagwError::validation(format!(
                        "plugin '{plugin_ref}' is registered as a {} plugin",
                        plugin.plugin_type.as_str()
                    )));
                }
                if plugin.plugin_type == PluginKind::Auth {
                    return Err(OagwError::validation(
                        "auth plugins are bound through `auth.type`, not `plugins.items`",
                    ));
                }
                continue;
            }

            let (base, _) = gts_helpers::split_gts(plugin_ref).ok_or_else(|| {
                OagwError::validation(format!("plugin reference '{plugin_ref}' is not a GTS identifier"))
            })?;
            match PluginKind::from_base_type(base) {
                Some(PluginKind::Guard) => {
                    if !self.catalog.has_guard(plugin_ref) {
                        return Err(OagwError::validation(format!(
                            "unknown guard plugin: {plugin_ref}"
                        )));
                    }
                }
                Some(PluginKind::Transform) => {
                    if !self.catalog.has_transform(plugin_ref) {
                        return Err(OagwError::validation(format!(
                            "unknown transform plugin: {plugin_ref}"
                        )));
                    }
                }
                Some(PluginKind::Auth) => {
                    return Err(OagwError::validation(
                        "auth plugins are bound through `auth.type`, not `plugins.items`",
                    ));
                }
                None => {
                    return Err(OagwError::validation(format!(
                        "plugin reference '{plugin_ref}' is not an OAGW plugin identifier"
                    )));
                }
            }
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Free helpers
// ---------------------------------------------------------------------------

/// Structured configuration-change audit record (DESIGN.md §4.3).
fn audit_config_change(event: &'static str, tenant_id: Uuid, id: Uuid, alias: Option<&str>) {
    tracing::info!(
        target: "oagw.audit",
        event,
        tenant_id = %tenant_id,
        resource_id = %id,
        alias = alias.unwrap_or_default(),
        "oagw configuration changed"
    );
}

/// Ensure a path starts with `/` and carries no trailing slash (except root).
#[must_use]
pub fn normalize_path(path: &str) -> String {
    let trimmed = path.trim();
    let with_slash = if trimmed.starts_with('/') {
        trimmed.to_owned()
    } else {
        format!("/{trimmed}")
    };
    if with_slash.len() > 1 {
        with_slash.trim_end_matches('/').to_owned()
    } else {
        with_slash
    }
}

/// Longest-prefix semantics: the request path equals the route path or
/// continues it at a segment boundary.
#[must_use]
pub fn path_matches(request_path: &str, route_path: &str) -> bool {
    if route_path == "/" {
        return true;
    }
    if request_path == route_path {
        return true;
    }
    request_path
        .strip_prefix(route_path)
        .is_some_and(|rest| rest.starts_with('/'))
}

fn validate_protocol(protocol: &str) -> OagwResult<String> {
    if protocol == gts_helpers::PROTOCOL_HTTP || protocol == gts_helpers::PROTOCOL_GRPC {
        return Ok(protocol.to_owned());
    }
    Err(OagwError::validation(format!(
        "protocol must be '{}' or '{}', got '{protocol}'",
        gts_helpers::PROTOCOL_HTTP,
        gts_helpers::PROTOCOL_GRPC
    )))
}

fn build_endpoints(input: &UpstreamWriteInput) -> OagwResult<Vec<Endpoint>> {
    if input.server.endpoints.is_empty() {
        return Err(OagwError::validation(
            "server.endpoints must contain at least one endpoint",
        ));
    }
    Ok(input
        .server
        .endpoints
        .iter()
        .map(|ep| Endpoint {
            scheme: ep.scheme,
            host: alias::normalize_host(&ep.host),
            port: ep.port.unwrap_or_else(|| ep.scheme.standard_port()),
        })
        .collect())
}

fn validate_tags(tags: Vec<String>) -> OagwResult<Vec<String>> {
    for tag in &tags {
        if tag.is_empty()
            || !tag
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
        {
            return Err(OagwError::validation(format!(
                "tag '{tag}' does not match ^[a-z0-9_-]+$"
            )));
        }
    }
    Ok(tags)
}

fn validate_cors(cors: Option<&crate::domain::model::CorsConfig>) -> OagwResult<()> {
    let Some(cors) = cors else { return Ok(()) };
    if cors.allow_credentials && cors.allowed_origins.iter().any(|o| o == "*") {
        return Err(OagwError::validation(
            "Cannot use allow_credentials with wildcard origin",
        ));
    }
    for method in &cors.allowed_methods {
        if !matches!(
            method.to_ascii_uppercase().as_str(),
            "GET" | "POST" | "PUT" | "PATCH" | "DELETE" | "HEAD" | "OPTIONS"
        ) {
            return Err(OagwError::validation(format!(
                "cors.allowed_methods contains an unsupported method '{method}'"
            )));
        }
    }
    Ok(())
}

const ALLOWED_ROUTE_METHODS: &[&str] = &["GET", "POST", "PUT", "DELETE", "PATCH"];

fn validate_match(match_config: &MatchConfig, upstream: &Upstream) -> OagwResult<MatchConfig> {
    match (&match_config.http, &match_config.grpc) {
        (Some(_), Some(_)) | (None, None) => {
            return Err(OagwError::validation(
                "match must contain exactly one of {http|grpc}",
            ));
        }
        (Some(http), None) => {
            if upstream.is_grpc() {
                return Err(OagwError::validation(
                    "upstream protocol is gRPC; the route must declare a `grpc` match",
                ));
            }
            validate_http_match(http)?;
        }
        (None, Some(grpc)) => {
            if !upstream.is_grpc() {
                return Err(OagwError::validation(
                    "upstream protocol is HTTP; the route must declare an `http` match",
                ));
            }
            if grpc.service.trim().is_empty() || grpc.method.trim().is_empty() {
                return Err(OagwError::validation(
                    "grpc match requires non-empty `service` and `method`",
                ));
            }
        }
    }
    Ok(match_config.clone())
}

fn validate_http_match(http: &HttpMatch) -> OagwResult<()> {
    if http.methods.is_empty() {
        return Err(OagwError::validation(
            "match.http.methods must contain at least one method",
        ));
    }
    for method in &http.methods {
        if !ALLOWED_ROUTE_METHODS
            .iter()
            .any(|m| m.eq_ignore_ascii_case(method))
        {
            return Err(OagwError::validation(format!(
                "match.http.methods contains an unsupported method '{method}'"
            )));
        }
    }
    if http.path.trim().is_empty() {
        return Err(OagwError::validation(
            "match.http.path must not be empty",
        ));
    }
    Ok(())
}

/// Route match determinism: no two enabled routes under one upstream may share
/// `(path_prefix, priority)` for the same method.
fn ensure_match_unique(
    siblings: &[Route],
    ignore_id: Option<Uuid>,
    match_config: &MatchConfig,
    priority: i32,
) -> OagwResult<()> {
    let Some(http) = match_config.http.as_ref() else {
        // gRPC: `(service, method)` must be unique.
        let Some(grpc) = match_config.grpc.as_ref() else {
            return Ok(());
        };
        for route in siblings {
            if Some(route.id) == ignore_id {
                continue;
            }
            if route
                .match_config
                .grpc
                .as_ref()
                .is_some_and(|g| g.service == grpc.service && g.method == grpc.method)
            {
                return Err(OagwError::new(
                    ErrorKind::Conflict,
                    format!(
                        "a route for {}/{} already exists on this upstream",
                        grpc.service, grpc.method
                    ),
                ));
            }
        }
        return Ok(());
    };

    let path = normalize_path(&http.path);
    for route in siblings {
        if Some(route.id) == ignore_id || route.priority != priority {
            continue;
        }
        let Some(other) = route.match_config.http.as_ref() else {
            continue;
        };
        if normalize_path(&other.path) != path {
            continue;
        }
        let overlap = http.methods.iter().any(|m| {
            other
                .methods
                .iter()
                .any(|o| o.eq_ignore_ascii_case(m))
        });
        if overlap {
            return Err(OagwError::new(
                ErrorKind::Conflict,
                format!(
                    "a route with path '{path}' and priority {priority} already exists for one of \
                     the requested methods on this upstream"
                ),
            ));
        }
    }
    Ok(())
}

/// Convenience: JSON projection of an upstream for list/get responses.
#[must_use]
pub fn upstream_json(upstream: &Upstream) -> Value {
    serde_json::to_value(upstream).unwrap_or(Value::Null)
}

/// Convenience: JSON projection of a route.
#[must_use]
pub fn route_json(route: &Route) -> Value {
    serde_json::to_value(route).unwrap_or(Value::Null)
}

/// Convenience: JSON projection of a plugin (never includes `source_code`).
#[must_use]
pub fn plugin_json(plugin: &Plugin) -> Value {
    let mut value = serde_json::to_value(plugin).unwrap_or(Value::Null);
    if let Some(obj) = value.as_object_mut() {
        obj.insert("id".to_owned(), Value::String(plugin.gts_id()));
        obj.insert("uuid".to_owned(), Value::String(plugin.id.to_string()));
    }
    value
}

#[cfg(test)]
#[path = "management_tests.rs"]
mod tests;
