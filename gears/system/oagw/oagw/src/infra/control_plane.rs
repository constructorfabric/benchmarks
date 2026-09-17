//! In-memory control plane implementation.

use std::collections::BTreeSet;
use std::sync::Arc;

use async_trait::async_trait;
use dashmap::mapref::entry::Entry;
use uuid::Uuid;

use super::storage::Services;
use super::plugins::classify_binding;
use crate::domain::alias;
use crate::domain::control_plane::{
    Caller, ControlPlaneService, ListOptions, RouteResolution, UpstreamResolution,
};
use crate::domain::model::{
    AuthConfig, CorsConfig, MatchConfig, PathSuffixMode, PluginBinding, PluginChainConfig,
    PluginDef, PluginKind, RateLimitConfig, Route, SharingMode, Upstream,
};
use crate::domain::wire::{PluginInput, RouteInput, UpstreamInput};
use crate::error::OagwError;
use crate::gts_helpers;

/// Factor common error construction.
fn validation(detail: impl Into<String>) -> OagwError {
    OagwError::validation(detail)
}

const KNOWN_SCHEMES: &[&str] = &["https", "wss", "wt", "grpc", "http"];

/// Validate and normalize an endpoint pool.
fn validate_endpoints(endpoints: &[crate::domain::model::Endpoint]) -> Result<(), OagwError> {
    if endpoints.is_empty() {
        return Err(validation("endpoints must contain at least one endpoint"));
    }
    let first_scheme = &endpoints[0].scheme;
    let first_port = endpoints[0].port;
    for (i, ep) in endpoints.iter().enumerate() {
        if !KNOWN_SCHEMES.contains(&ep.scheme.as_str()) {
            return Err(validation(format!(
                "endpoint {} has unsupported scheme `{}`",
                i, ep.scheme
            )));
        }
        if ep.port == 0 {
            return Err(validation(format!("endpoint {} has port 0", i)));
        }
        alias::validate_hostname(&ep.host).map_err(validation)?;
    }
    // Pool invariant: identical scheme + port across all endpoints.
    if endpoints
        .iter()
        .any(|ep| ep.scheme != *first_scheme || ep.port != first_port)
    {
        return Err(validation(
            "all endpoints in a pool must share the same scheme and port",
        ));
    }
    Ok(())
}

fn validate_rate_limit(cfg: &RateLimitConfig) -> Result<(), OagwError> {
    if cfg.sustained.rate < 1 {
        return Err(validation("rate_limit.sustained.rate must be >= 1"));
    }
    if cfg.cost < 1 {
        return Err(validation("rate_limit.cost must be >= 1"));
    }
    if let Some(b) = &cfg.burst {
        if b.capacity == 0 {
            // Capacity 0 is the model's "defaults to the sustained rate"
            // sentinel handled by the limiter; nothing to validate.
            return Ok(());
        }
    }
    Ok(())
}

fn validate_cors(cfg: &CorsConfig) -> Result<(), OagwError> {
    if cfg.allow_credentials && cfg.allowed_origins.iter().any(|o| o == "*") {
        return Err(validation(
            "cors.allow_credentials cannot be true when allowed_origins contains \"*\"",
        ));
    }
    for origin in &cfg.allowed_origins {
        if origin != "*" && !origin.contains("://") {
            return Err(validation(format!("cors origin `{origin}` must be an absolute URI")));
        }
    }
    Ok(())
}

fn validate_auth(cfg: &AuthConfig, store: &Services) -> Result<(), OagwError> {
    if cfg.auth_type.is_empty() {
        return Ok(());
    }
    // Named auth plugins: must be resolvable via the builtin registry.
    match store.auth_plugins.resolve(&cfg.auth_type) {
        Ok(_) => Ok(()),
        Err(OagwError::PluginNotFound { .. }) => {
            // UUID-backed custom auth plugin: must exist in the store.
            if let Some(uuid) = parse_custom_plugin_uuid(&cfg.auth_type, PluginKind::Auth) {
                if store.plugins.contains_key(&uuid) {
                    return Ok(());
                }
                return Err(validation(format!(
                    "unknown auth plugin `{}`",
                    cfg.auth_type
                )));
            }
            Err(validation(format!("unknown auth plugin `{}`", cfg.auth_type)))
        }
        Err(e) => Err(e),
    }
}

/// If `gts` is a `{type}~{uuid}` custom plugin reference of `kind`, return
/// the UUID.
fn parse_custom_plugin_uuid(gts: &str, kind: PluginKind) -> Option<Uuid> {
    let prefix = format!("{}~", kind.resource_type());
    let instance = gts.strip_prefix(&prefix)?;
    Uuid::parse_str(instance).ok()
}

fn validate_plugin_chain(
    chain: &PluginChainConfig,
    store: &Services,
) -> Result<(), OagwError> {
    for item in &chain.items {
        match classify_binding(&item.plugin_ref) {
            Some(PluginKind::Auth) => {
                // Auth is configured via the dedicated `auth` block (one per
                // upstream, DESIGN.md plugin system); the plugins chain only
                // carries guard + transform bindings.
                return Err(validation(format!(
                    "plugin `{}` is an auth plugin; auth is configured via the `auth` block",
                    item.plugin_ref
                )));
            }
            Some(PluginKind::Guard) => {
                if store.guard_resolvable(&item.plugin_ref).is_err()
                    && !is_custom_guard(&item.plugin_ref, store)
                {
                    return Err(validation(format!(
                        "unknown guard plugin `{}`",
                        item.plugin_ref
                    )));
                }
            }
            Some(PluginKind::Transform) => {
                if store.transform_resolvable(&item.plugin_ref).is_err()
                    && !is_custom_transform(&item.plugin_ref, store)
                {
                    return Err(validation(format!(
                        "unknown transform plugin `{}`",
                        item.plugin_ref
                    )));
                }
            }
            None => {
                return Err(validation(format!(
                    "plugin `{}` is not a valid OAGW plugin identifier",
                    item.plugin_ref
                )));
            }
        }
    }
    Ok(())
}

fn validate_auth_binding(gts: &str, store: &Services) -> Result<(), OagwError> {
    if store.auth_plugins.resolve(gts).is_ok() {
        return Ok(());
    }
    if let Some(uuid) = parse_custom_plugin_uuid(gts, PluginKind::Auth) {
        if store.plugins.contains_key(&uuid) {
            return Ok(());
        }
    }
    Err(validation(format!("unknown auth plugin `{gts}`")))
}

fn is_custom_guard(gts: &str, store: &Services) -> bool {
    parse_custom_plugin_uuid(gts, PluginKind::Guard)
        .map(|uuid| store.plugins.contains_key(&uuid))
        .unwrap_or(false)
}

fn is_custom_transform(gts: &str, store: &Services) -> bool {
    parse_custom_plugin_uuid(gts, PluginKind::Transform)
        .map(|uuid| store.plugins.contains_key(&uuid))
        .unwrap_or(false)
}

impl Services {
    fn guard_resolvable(&self, gts: &str) -> Result<(), OagwError> {
        super::plugins::GuardPluginRegistry::resolve(gts).map(|_| ())
    }

    fn transform_resolvable(&self, gts: &str) -> Result<(), OagwError> {
        super::plugins::TransformPluginRegistry::resolve(gts).map(|_| ())
    }
}

/// The set of HTTP methods a route's match allows.
fn match_methods(m: &MatchConfig) -> Vec<String> {
    match m {
        MatchConfig::Http(h) => h.methods.clone(),
        MatchConfig::Grpc(_) => Vec::new(),
    }
}

fn route_methods_overlap(a: &[String], b: &[String]) -> bool {
    let set: BTreeSet<&str> = a.iter().map(String::as_str).collect();
    b.iter().any(|m| set.contains(m.as_str()))
}

#[async_trait]
impl ControlPlaneService for Arc<Services> {
    // -- upstreams ----------------------------------------------------------

    async fn create_upstream(
        &self,
        caller: &Caller,
        input: UpstreamInput,
    ) -> Result<Upstream, OagwError> {
        if !caller.has_permission(gts_helpers::PERM_UPSTREAM_CREATE) {
            return Err(OagwError::Forbidden {
                detail: "missing permission gts.cf.core.oagw.upstream.v1~:create".into(),
            });
        }
        validate_endpoints(&input.server.endpoints)?;
        if let Some(rl) = &input.rate_limit {
            validate_rate_limit(rl)?;
        }
        validate_cors(&input.cors)?;
        if let Some(auth) = &input.auth {
            validate_auth(auth, self)?;
        }
        validate_plugin_chain(&input.plugins, self)?;

        let computed = alias::resolve_alias_for_create(&input.server.endpoints, input.alias.as_deref())
            .map_err(validation)?;

        let now = now_secs();
        let upstream = Upstream {
            id: Uuid::new_v4(),
            tenant_id: caller.tenant_id,
            enabled: input.enabled,
            alias: computed,
            tags: input.tags.clone(),
            server: input.server.clone(),
            protocol: input.protocol,
            auth: input.auth.clone(),
            headers: input.headers.clone(),
            plugins: input.plugins.clone(),
            rate_limit: input.rate_limit.clone(),
            cors: input.cors.clone(),
            created_at: now,
            updated_at: now,
        };

        insert_upstream_unique(self, upstream.clone())?;
        Ok(upstream)
    }

    async fn get_upstream(&self, caller: &Caller, id: Uuid) -> Result<Upstream, OagwError> {
        if !caller.has_permission(gts_helpers::PERM_UPSTREAM_READ) {
            return Err(OagwError::Forbidden {
                detail: "missing permission gts.cf.core.oagw.upstream.v1~:read".into(),
            });
        }
        self.upstreams
            .get(&id)
            .map(|r| r.clone())
            .filter(|r| r.tenant_id == caller.tenant_id)
            .ok_or_else(|| OagwError::NotFound {
                detail: "upstream not found".into(),
            })
    }

    async fn list_upstreams(
        &self,
        caller: &Caller,
        opts: &ListOptions,
    ) -> Result<(Vec<Upstream>, usize), OagwError> {
        if !caller.has_permission(gts_helpers::PERM_UPSTREAM_READ) {
            return Err(OagwError::Forbidden {
                detail: "missing permission gts.cf.core.oagw.upstream.v1~:read".into(),
            });
        }
        let mut all: Vec<Upstream> = self
            .upstreams
            .iter()
            .filter(|r| r.tenant_id == caller.tenant_id)
            .map(|r| r.clone())
            .collect();
        all.sort_by_key(|u| u.created_at);
        let total = all.len();
        let opts = opts.clone().normalized();
        Ok((all.into_iter().skip(opts.skip).take(opts.top).collect(), total))
    }

    async fn update_upstream(
        &self,
        caller: &Caller,
        id: Uuid,
        input: UpstreamInput,
    ) -> Result<Upstream, OagwError> {
        if !caller.has_permission(gts_helpers::PERM_UPSTREAM_OVERRIDE) {
            return Err(OagwError::Forbidden {
                detail: "missing permission gts.cf.core.oagw.upstream.v1~:override".into(),
            });
        }
        let existing = self
            .upstreams
            .get(&id)
            .map(|r| r.clone())
            .filter(|r| r.tenant_id == caller.tenant_id)
            .ok_or_else(|| OagwError::NotFound {
                detail: "upstream not found".into(),
            })?;

        validate_endpoints(&input.server.endpoints)?;
        if let Some(rl) = &input.rate_limit {
            validate_rate_limit(rl)?;
        }
        validate_cors(&input.cors)?;
        if let Some(auth) = &input.auth {
            validate_auth(auth, self)?;
        }
        validate_plugin_chain(&input.plugins, self)?;

        let new_alias = alias::enforce_alias_update(
            &existing.alias,
            &existing.server.endpoints,
            input.alias.as_deref(),
            &input.server.endpoints,
        )
        .map_err(validation)?;

        if new_alias != existing.alias {
            if self
                .upstream_by_alias
                .get(&(caller.tenant_id, new_alias.clone()))
                .map(|v| *v != id)
                .unwrap_or(false)
            {
                return Err(OagwError::Conflict {
                    detail: format!("upstream alias `{new_alias}` already exists for this tenant"),
                });
            }
        }

        let updated = Upstream {
            id,
            tenant_id: caller.tenant_id,
            enabled: input.enabled,
            alias: new_alias.clone(),
            tags: input.tags.clone(),
            server: input.server.clone(),
            protocol: input.protocol,
            auth: input.auth.clone(),
            headers: input.headers.clone(),
            plugins: input.plugins.clone(),
            rate_limit: input.rate_limit.clone(),
            cors: input.cors.clone(),
            created_at: existing.created_at,
            updated_at: now_secs(),
        };

        if updated != existing {
            // Re-index alias only when it changed.
            if existing.alias != updated.alias {
                self.upstream_by_alias.remove(&(caller.tenant_id, existing.alias.clone()));
                self.upstream_by_alias
                    .insert((caller.tenant_id, new_alias), id);
            }
            self.upstreams.insert(id, updated.clone());
        }
        Ok(updated)
    }

    async fn delete_upstream(&self, caller: &Caller, id: Uuid) -> Result<(), OagwError> {
        if !caller.has_permission(gts_helpers::PERM_UPSTREAM_DELETE) {
            return Err(OagwError::Forbidden {
                detail: "missing permission gts.cf.core.oagw.upstream.v1~:delete".into(),
            });
        }
        let existing = self
            .upstreams
            .get(&id)
            .map(|r| r.clone())
            .filter(|r| r.tenant_id == caller.tenant_id)
            .ok_or_else(|| OagwError::NotFound {
                detail: "upstream not found".into(),
            })?;

        // Cascade: routes referencing this upstream are deleted too.
        let route_ids: Vec<Uuid> = self
            .routes
            .iter()
            .filter(|r| r.upstream_id == id)
            .map(|r| r.id)
            .collect();
        for rid in route_ids {
            self.routes.remove(&rid);
        }
        self.upstream_by_alias
            .remove(&(caller.tenant_id, existing.alias.clone()));
        self.upstreams.remove(&id);
        Ok(())
    }

    // -- routes -------------------------------------------------------------

    async fn create_route(
        &self,
        caller: &Caller,
        input: RouteInput,
    ) -> Result<Route, OagwError> {
        if !caller.has_permission(gts_helpers::PERM_ROUTE_CREATE) {
            return Err(OagwError::Forbidden {
                detail: "missing permission gts.cf.core.oagw.route.v1~:create".into(),
            });
        }
        let upstream_id = input.upstream_id;
        // Upstream must be owned by the caller's tenant (ancestors are not
        // addressable).
        self.upstreams
            .get(&upstream_id)
            .map(|r| r.tenant_id)
            .filter(|t| *t == caller.tenant_id)
            .ok_or_else(|| {
                validation("route references an upstream not owned by the calling tenant")
            })?;

        validate_route_input(&input)?;
        validate_plugin_chain(&input.plugins, self)?;
        check_match_uniqueness(self, caller.tenant_id, upstream_id, None, &input.match_config)?;

        let now = now_secs();
        let route = Route {
            id: Uuid::new_v4(),
            tenant_id: caller.tenant_id,
            upstream_id,
            enabled: input.enabled,
            tags: input.tags.clone(),
            match_config: input.match_config.clone(),
            plugins: input.plugins.clone(),
            rate_limit: input.rate_limit.clone(),
            created_at: now,
            updated_at: now,
        };
        self.routes.insert(route.id, route.clone());
        Ok(route)
    }

    async fn get_route(&self, caller: &Caller, id: Uuid) -> Result<Route, OagwError> {
        if !caller.has_permission(gts_helpers::PERM_ROUTE_READ) {
            return Err(OagwError::Forbidden {
                detail: "missing permission gts.cf.core.oagw.route.v1~:read".into(),
            });
        }
        self.routes
            .get(&id)
            .map(|r| r.clone())
            .filter(|r| r.tenant_id == caller.tenant_id)
            .ok_or_else(|| OagwError::NotFound {
                detail: "route not found".into(),
            })
    }

    async fn list_routes(
        &self,
        caller: &Caller,
        opts: &ListOptions,
    ) -> Result<(Vec<Route>, usize), OagwError> {
        if !caller.has_permission(gts_helpers::PERM_ROUTE_READ) {
            return Err(OagwError::Forbidden {
                detail: "missing permission gts.cf.core.oagw.route.v1~:read".into(),
            });
        }
        let mut all: Vec<Route> = self
            .routes
            .iter()
            .filter(|r| r.tenant_id == caller.tenant_id)
            .map(|r| r.clone())
            .collect();
        all.sort_by_key(|r| r.created_at);
        let total = all.len();
        let opts = opts.clone().normalized();
        Ok((all.into_iter().skip(opts.skip).take(opts.top).collect(), total))
    }

    async fn update_route(
        &self,
        caller: &Caller,
        id: Uuid,
        input: RouteInput,
    ) -> Result<Route, OagwError> {
        if !caller.has_permission(gts_helpers::PERM_ROUTE_OVERRIDE) {
            return Err(OagwError::Forbidden {
                detail: "missing permission gts.cf.core.oagw.route.v1~:override".into(),
            });
        }
        let existing = self
            .routes
            .get(&id)
            .map(|r| r.clone())
            .filter(|r| r.tenant_id == caller.tenant_id)
            .ok_or_else(|| OagwError::NotFound {
                detail: "route not found".into(),
            })?;

        // upstream_id is immutable: the payload value must match the stored one.
        if input.upstream_id != existing.upstream_id {
            return Err(validation("route upstream_id is immutable"));
        }

        validate_route_input(&input)?;
        validate_plugin_chain(&input.plugins, self)?;
        check_match_uniqueness(
            self,
            caller.tenant_id,
            existing.upstream_id,
            Some(id),
            &input.match_config,
        )?;

        let updated = Route {
            id,
            tenant_id: caller.tenant_id,
            upstream_id: existing.upstream_id,
            enabled: input.enabled,
            tags: input.tags.clone(),
            match_config: input.match_config.clone(),
            plugins: input.plugins.clone(),
            rate_limit: input.rate_limit.clone(),
            created_at: existing.created_at,
            updated_at: now_secs(),
        };
        if updated != existing {
            self.routes.insert(id, updated.clone());
        }
        Ok(updated)
    }

    async fn delete_route(&self, caller: &Caller, id: Uuid) -> Result<(), OagwError> {
        if !caller.has_permission(gts_helpers::PERM_ROUTE_DELETE) {
            return Err(OagwError::Forbidden {
                detail: "missing permission gts.cf.core.oagw.route.v1~:delete".into(),
            });
        }
        let exists = self
            .routes
            .get(&id)
            .map(|r| r.tenant_id == caller.tenant_id)
            .unwrap_or(false);
        if !exists {
            return Err(OagwError::NotFound {
                detail: "route not found".into(),
            });
        }
        self.routes.remove(&id);
        Ok(())
    }

    // -- custom plugins -----------------------------------------------------

    async fn create_plugin(
        &self,
        caller: &Caller,
        input: PluginInput,
    ) -> Result<PluginDef, OagwError> {
        let perm = match input.plugin_type {
            PluginKind::Auth => gts_helpers::PERM_AUTH_PLUGIN_CREATE,
            PluginKind::Guard => gts_helpers::PERM_GUARD_PLUGIN_CREATE,
            PluginKind::Transform => gts_helpers::PERM_TRANSFORM_PLUGIN_CREATE,
        };
        if !caller.has_permission(perm) {
            return Err(OagwError::Forbidden {
                detail: format!("missing permission {perm}"),
            });
        }
        if input.name.trim().is_empty() {
            return Err(validation("plugin name must not be empty"));
        }
        let name_conflict = self
            .plugins
            .iter()
            .any(|p| p.tenant_id == caller.tenant_id && p.name == input.name);
        if name_conflict {
            return Err(OagwError::Conflict {
                detail: format!("a plugin named `{}` already exists for this tenant", input.name),
            });
        }

        let plugin = PluginDef {
            id: Uuid::new_v4(),
            tenant_id: caller.tenant_id,
            name: input.name.clone(),
            description: input.description.clone(),
            plugin_type: input.plugin_type,
            phases: input.phases.clone(),
            config_schema: input.config_schema.clone(),
            source_code: input.source_code.clone(),
        };
        let gts_id = plugin.gts_id();
        self.plugins.insert(plugin.id, plugin.clone());
        self.plugin_by_gts.insert(gts_id, plugin.id);
        Ok(plugin)
    }

    async fn get_plugin(&self, caller: &Caller, id: Uuid) -> Result<PluginDef, OagwError> {
        let perm = plugin_read_perm(id, self);
        if !caller.has_permission(perm) {
            return Err(OagwError::Forbidden {
                detail: format!("missing permission {perm}"),
            });
        }
        self.plugins
            .get(&id)
            .map(|p| p.clone())
            .filter(|p| p.tenant_id == caller.tenant_id)
            .ok_or_else(|| OagwError::NotFound {
                detail: "plugin not found".into(),
            })
    }

    async fn list_plugins(
        &self,
        caller: &Caller,
        opts: &ListOptions,
    ) -> Result<(Vec<PluginDef>, usize), OagwError> {
        // List requires any plugin read permission.
        let readable = [
            gts_helpers::PERM_AUTH_PLUGIN_READ,
            gts_helpers::PERM_GUARD_PLUGIN_READ,
            gts_helpers::PERM_TRANSFORM_PLUGIN_READ,
        ]
        .iter()
        .any(|perm| caller.has_permission(perm));
        if !readable {
            return Err(OagwError::Forbidden {
                detail: "missing plugin read permission".into(),
            });
        }
        let mut all: Vec<PluginDef> = self
            .plugins
            .iter()
            .filter(|p| p.tenant_id == caller.tenant_id)
            .map(|p| p.clone())
            .collect();
        all.sort_by_key(|p| p.name.clone());
        let total = all.len();
        let opts = opts.clone().normalized();
        Ok((all.into_iter().skip(opts.skip).take(opts.top).collect(), total))
    }

    async fn delete_plugin(&self, caller: &Caller, id: Uuid) -> Result<(), OagwError> {
        let existing = {
            let perm = plugin_delete_perm_for(id, self);
            if !caller.has_permission(perm) {
                return Err(OagwError::Forbidden {
                    detail: format!("missing permission {perm}"),
                });
            }
            self.plugins
                .get(&id)
                .map(|p| p.clone())
                .filter(|p| p.tenant_id == caller.tenant_id)
                .ok_or_else(|| OagwError::NotFound {
                    detail: "plugin not found".into(),
                })?
        };

        // 409 when referenced by any upstream/route binding.
        let mut referenced_upstreams = Vec::new();
        let mut referenced_routes = Vec::new();
        let gts_id = existing.gts_id();
        for u in self.upstreams.iter() {
            if plugin_binding_references(&u.plugins, &gts_id)
                || u.auth.as_ref().map(|a| a.auth_type == gts_id).unwrap_or(false)
            {
                referenced_upstreams.push(u.id);
            }
        }
        for r in self.routes.iter() {
            if plugin_binding_references(&r.plugins, &gts_id) {
                referenced_routes.push(r.id);
            }
        }
        if !referenced_upstreams.is_empty() || !referenced_routes.is_empty() {
            return Err(OagwError::Conflict {
                detail: format!(
                    "plugin is in use — referenced_by upstreams {:?}, routes {:?}",
                    referenced_upstreams, referenced_routes
                ),
            });
        }

        self.plugin_by_gts.remove(&gts_id);
        self.plugins.remove(&id);
        Ok(())
    }

    async fn get_plugin_source(
        &self,
        caller: &Caller,
        id: Uuid,
    ) -> Result<Option<String>, OagwError> {
        let perm = plugin_read_perm(id, self);
        if !caller.has_permission(perm) {
            return Err(OagwError::Forbidden {
                detail: format!("missing permission {perm}"),
            });
        }
        self.plugins
            .get(&id)
            .map(|p| p.source_code.clone())
            .filter(|_| self.plugins.get(&id).map(|p| p.tenant_id == caller.tenant_id).unwrap_or(false))
            .ok_or_else(|| OagwError::NotFound {
                detail: "plugin not found".into(),
            })
    }

    // -- routing resolution -------------------------------------------------

    async fn resolve_alias(
        &self,
        caller: &Caller,
        alias: &str,
    ) -> Result<UpstreamResolution, OagwError> {
        let alias_norm = alias::normalize_alias(alias);
        let chain = self.tenant_chain.chain(caller).await.map_err(|e| e)?;

        let mut selected: Option<Upstream> = None;
        let mut ancestors: Vec<Upstream> = Vec::new();
        for tenant in chain {
            if let Some(id) = self.upstream_by_alias.get(&(tenant, alias_norm.clone())) {
                let id = *id;
                if let Some(rec) = self.upstreams.get(&id).map(|r| r.clone()) {
                    if selected.is_none() {
                        selected = Some(rec);
                    } else {
                        ancestors.push(rec);
                    }
                }
            }
        }
        let upstream = selected.ok_or_else(|| OagwError::NotFound {
            detail: format!("no upstream is bound to alias `{alias_norm}`"),
        })?;
        if !upstream.enabled {
            return Err(OagwError::LinkUnavailable {
                detail: format!("upstream `{alias_norm}` is disabled"),
            });
        }
        Ok(UpstreamResolution { upstream, ancestors })
    }

    async fn resolve_route(
        &self,
        caller: &Caller,
        resolution: &UpstreamResolution,
        method: &str,
        path: &str,
        query_keys: &[String],
    ) -> Result<Option<RouteResolution>, OagwError> {
        let _ = caller;
        let upstream_id = resolution.upstream.id;
        // (match_len, route, suffix, suffix_mode); longest prefix wins.
        let mut best: Option<(usize, Route, String, PathSuffixMode)> = None;
        let path = path.trim_start_matches('/');

        for r in self.routes.iter() {
            let r = r.clone();
            if r.upstream_id != upstream_id || !r.enabled {
                continue;
            }
            let (methods, route_path, suffix_mode) = match &r.match_config {
                MatchConfig::Http(h) => (&h.methods, &h.path, h.path_suffix_mode),
                MatchConfig::Grpc(_) => continue, // no gRPC proxy path (Phase 3)
            };
            if !methods.iter().any(|m| m.eq_ignore_ascii_case(method)) {
                continue;
            }
            let route_path = route_path.trim_start_matches('/');
            if !path.starts_with(route_path) {
                continue;
            }
            let suffix = path[route_path.len()..].to_owned();
            let suffix = suffix.strip_prefix('/').unwrap_or(&suffix).to_owned();
            if best.as_ref().map(|(len, _, _, _)| route_path.len() > *len).unwrap_or(true) {
                best = Some((route_path.len(), r, suffix, suffix_mode));
            }
        }

        let (_, route, suffix, suffix_mode) = match best {
            Some(b) => b,
            None => return Ok(None),
        };

        // Guard rule (DESIGN.md): a path suffix with `path_suffix_mode:
        // disabled` on the matched route is rejected.
        if !suffix.is_empty() && suffix_mode == PathSuffixMode::Disabled {
            return Err(OagwError::validation(
                "path suffix is not allowed for this route (path_suffix_mode: disabled)",
            ));
        }

        // Query allowlist guard: unknown query params are rejected (empty
        // allowlist = allow none).
        let allowlist: Vec<String> = match &route.match_config {
            MatchConfig::Http(h) => h.query_allowlist.clone(),
            MatchConfig::Grpc(_) => Vec::new(),
        };
        let allow: std::collections::HashSet<&str> =
            allowlist.iter().map(String::as_str).collect();
        for key in query_keys {
            if !allow.contains(key.as_str()) {
                return Err(OagwError::validation(format!(
                    "query parameter `{key}` is not allowed by this route (query_allowlist: {allowlist:?})"
                )));
            }
        }

        Ok(Some(RouteResolution {
            route,
            path_suffix: suffix,
            query_allowlist: allowlist,
        }))
    }

    fn rate_limit_chain(
        &self,
        resolution: &UpstreamResolution,
        route_limit: Option<&RateLimitConfig>,
    ) -> Vec<RateLimitConfig> {
        let mut chain = Vec::new();
        if let Some(l) = &resolution.upstream.rate_limit {
            chain.push(l.clone());
        }
        for anc in &resolution.ancestors {
            if let Some(l) = &anc.rate_limit {
                if l.sharing != SharingMode::Private {
                    chain.push(l.clone());
                }
            }
        }
        if let Some(l) = route_limit {
            chain.push(l.clone());
        }
        chain
    }

    fn effective_cors(&self, resolution: &UpstreamResolution) -> CorsConfig {
        let mut contribs: Vec<&CorsConfig> = resolution
            .ancestors
            .iter()
            .rev() // root first
            .filter(|a| a.cors.sharing != SharingMode::Private)
            .map(|a| &a.cors)
            .collect();
        contribs.push(&resolution.upstream.cors);

        let mut out = CorsConfig {
            sharing: SharingMode::Private,
            enabled: false,
            allowed_origins: Vec::new(),
            allowed_methods: Vec::new(),
            expose_headers: Vec::new(),
            allow_credentials: false,
        };
        let push_unique = |v: &mut Vec<String>, item: &str| {
            if !v.iter().any(|x| x == item) {
                v.push(item.to_owned());
            }
        };
        for c in contribs {
            out.enabled |= c.enabled;
            out.allow_credentials |= c.allow_credentials;
            for o in &c.allowed_origins {
                push_unique(&mut out.allowed_origins, o);
            }
            for m in &c.allowed_methods {
                push_unique(&mut out.allowed_methods, m);
            }
            for h in &c.expose_headers {
                push_unique(&mut out.expose_headers, h);
            }
        }
        out
    }

    fn merged_plugins(
        &self,
        resolution: &UpstreamResolution,
        route_plugins: &PluginChainConfig,
    ) -> Vec<PluginBinding> {
        let mut out = Vec::new();
        for anc in resolution.ancestors.iter().rev() {
            if anc.plugins.sharing != SharingMode::Private {
                out.extend(anc.plugins.items.iter().cloned());
            }
        }
        out.extend(resolution.upstream.plugins.items.iter().cloned());
        out.extend(route_plugins.items.iter().cloned());
        out
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn insert_upstream_unique(store: &Services, upstream: Upstream) -> Result<(), OagwError> {
    match store.upstream_by_alias.entry((upstream.tenant_id, upstream.alias.clone())) {
        Entry::Occupied(_) => Err(OagwError::Conflict {
            detail: format!(
                "upstream alias `{}` already exists for this tenant",
                upstream.alias
            ),
        }),
        Entry::Vacant(v) => {
            v.insert(upstream.id);
            store.upstreams.insert(upstream.id, upstream);
            Ok(())
        }
    }
}

fn validate_route_input(input: &RouteInput) -> Result<(), OagwError> {
    match &input.match_config {
        MatchConfig::Http(h) => {
            if h.methods.is_empty() {
                return Err(validation("match.http.methods must contain at least one method"));
            }
            const ALLOWED: &[&str] = &["GET", "POST", "PUT", "PATCH", "DELETE"];
            for m in &h.methods {
                if !ALLOWED.iter().any(|a| a.eq_ignore_ascii_case(m)) {
                    return Err(validation(format!(
                        "match.http.methods contains unsupported method `{m}`"
                    )));
                }
            }
            if h.path.trim().is_empty() {
                return Err(validation("match.http.path must not be empty"));
            }
        }
        MatchConfig::Grpc(g) => {
            if g.service.trim().is_empty() || g.method.trim().is_empty() {
                return Err(validation(
                    "match.grpc requires non-empty service and method",
                ));
            }
        }
    }
    if let Some(rl) = &input.rate_limit {
        validate_rate_limit(rl)?;
    }
    Ok(())
}

fn check_match_uniqueness(
    store: &Services,
    tenant: Uuid,
    upstream_id: Uuid,
    self_id: Option<Uuid>,
    new_match: &MatchConfig,
) -> Result<(), OagwError> {
    let _ = tenant;
    for r in store.routes.iter() {
        if r.upstream_id != upstream_id || Some(r.id) == self_id {
            continue;
        }
        if match_config_clash(&r.match_config, new_match) {
            return Err(OagwError::Conflict {
                detail: format!(
                    "route matches an existing route under this upstream (path+method conflict)"
                ),
            });
        }
    }
    Ok(())
}

fn match_config_clash(a: &MatchConfig, b: &MatchConfig) -> bool {
    match (a, b) {
        (MatchConfig::Http(ha), MatchConfig::Http(hb)) => {
            ha.path.trim_start_matches('/') == hb.path.trim_start_matches('/')
                && route_methods_overlap(&ha.methods, &hb.methods)
        }
        (MatchConfig::Grpc(ga), MatchConfig::Grpc(gb)) => {
            ga.service == gb.service && ga.method == gb.method
        }
        _ => false,
    }
}

fn plugin_read_perm(id: Uuid, store: &Services) -> &'static str {
    match store
        .plugins
        .get(&id)
        .map(|p| p.plugin_type)
    {
        Some(PluginKind::Auth) => gts_helpers::PERM_AUTH_PLUGIN_READ,
        Some(PluginKind::Guard) => gts_helpers::PERM_GUARD_PLUGIN_READ,
        Some(PluginKind::Transform) => gts_helpers::PERM_TRANSFORM_PLUGIN_READ,
        None => gts_helpers::PERM_AUTH_PLUGIN_READ,
    }
}

fn plugin_delete_perm_for(id: Uuid, store: &Services) -> &'static str {
    match store
        .plugins
        .get(&id)
        .map(|p| p.plugin_type)
    {
        Some(PluginKind::Auth) => gts_helpers::PERM_AUTH_PLUGIN_DELETE,
        Some(PluginKind::Guard) => gts_helpers::PERM_GUARD_PLUGIN_DELETE,
        Some(PluginKind::Transform) => gts_helpers::PERM_TRANSFORM_PLUGIN_DELETE,
        None => gts_helpers::PERM_AUTH_PLUGIN_DELETE,
    }
}

fn plugin_binding_references(chain: &PluginChainConfig, gts_id: &str) -> bool {
    chain.items.iter().any(|b| b.plugin_ref == gts_id)
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use crate::config::OagwConfig;
    use crate::domain::control_plane::{
        Caller, ControlPlaneService, ListOptions,
    };
    use crate::domain::wire::{PluginInput, RouteInput, UpstreamInput};

    fn tenant() -> Uuid {
        Uuid::parse_str("aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa").unwrap()
    }

    fn caller() -> Caller {
        Caller {
            tenant_id: tenant(),
            subject_id: tenant(),
            scopes: vec!["*".to_owned()],
        }
    }

    fn svc() -> Arc<Services> {
        // `ControlPlaneService` is implemented for `Arc<Services>` (as the
        // handlers receive it via axum `Extension`).
        Arc::new(Services::empty(OagwConfig::default(), tenant()))
    }

    /// Deserialize an upstream input from JSON (exercises the wire contract).
    fn upstream_input(json: &str) -> UpstreamInput {
        serde_json::from_str(json).expect("upstream input JSON should deserialize")
    }

    fn http_upstream(alias: &str) -> UpstreamInput {
        upstream_input(&format!(
            r#"{{
                "alias": "{alias}",
                "protocol": "http",
                "server": {{"endpoints": [{{"scheme": "http", "host": "127.0.0.1", "port": 9080}}]}}
            }}"#
        ))
    }

    fn route_input(upstream_id: Uuid, path: &str) -> RouteInput {
        serde_json::from_value(serde_json::json!({
            "upstream_id": upstream_id,
            "match": {"http": {"methods": ["GET"], "path": path}},
        }))
        .expect("route input JSON should deserialize")
    }

    fn assert_status(err: OagwError, expect: u16) {
        assert_eq!(err.status().as_u16(), expect, "error: {err:?}");
    }

    #[tokio::test]
    async fn upstream_create_derives_alias_and_crud() {
        let s = svc();
        // IP-based endpoints require an explicit alias.
        let no_alias = upstream_input(
            r#"{"protocol":"http","server":{"endpoints":[{"scheme":"http","host":"10.0.0.9","port":80}]}}"#,
        );
        let err = s.create_upstream(&caller(), no_alias).await.err().unwrap();
        assert_status(err.clone(), 400);
        assert!(err.detail().contains("explicit alias is required"));

        let ip = upstream_input(
            r#"{"alias":"my-service","protocol":"http",
               "server":{"endpoints":[{"scheme":"http","host":"10.0.0.1","port":80}]}}"#,
        );
        let up = s.create_upstream(&caller(), ip).await.unwrap();
        assert_eq!(up.alias, "my-service");

        // Hostname endpoint: alias auto-derived to the hostname.
        let hn = upstream_input(
            r#"{"protocol":"http","server":{"endpoints":[{"scheme":"https","host":"api.openai.com"}]}}"#,
        );
        let up = s.create_upstream(&caller(), hn).await.unwrap();
        assert_eq!(up.alias, "api.openai.com");
        assert_eq!(up.server.endpoints[0].scheme, "https"); // default scheme

        // Duplicate alias is a 409 conflict.
        let dup = upstream_input(
            r#"{"alias":"my-service","protocol":"http",
               "server":{"endpoints":[{"scheme":"http","host":"10.0.0.2","port":80}]}}"#,
        );
        let err = s.create_upstream(&caller(), dup).await.err().unwrap();
        assert_status(err.clone(), 409);
        assert!(err.detail().contains("already exists"));

        // List sees two entries.
        let (items, total) = s.list_upstreams(&caller(), &ListOptions::default()).await.unwrap();
        assert_eq!(total, 2);
        assert_eq!(items.len(), 2);

        // Delete and confirm gone.
        s.delete_upstream(&caller(), up.id).await.unwrap();
        assert_status(s.get_upstream(&caller(), up.id).await.err().unwrap(), 404);
    }

    #[tokio::test]
    async fn upstream_update_enforces_immutable_alias_and_derivation() {
        let s = svc();
        let up = s.create_upstream(&caller(), http_upstream("svc-a")).await.unwrap();

        // Changing the alias is rejected.
        let mut changed = http_upstream("svc-b");
        changed.alias = Some("svc-c".to_owned());
        let err = s.update_upstream(&caller(), up.id, changed).await.err().unwrap();
        assert_status(err.clone(), 400);
        assert!(err.detail().contains("alias cannot be changed"));

        // Touching an unrelated field succeeds and keeps the alias.
        let mut touched = http_upstream("svc-a");
        touched.tags = vec!["t1".to_owned()];
        let updated = s.update_upstream(&caller(), up.id, touched).await.unwrap();
        assert_eq!(updated.alias, "svc-a");
        assert_eq!(updated.tags, vec!["t1".to_owned()]);
    }

    #[tokio::test]
    async fn plugin_chain_rejects_auth_and_unknown_bindings() {
        let s = svc();
        // Auth in the chain is a 400 — auth lives in the `auth` block.
        let auth_in_chain = upstream_input(
            r#"{"alias":"bad1","protocol":"http",
               "server":{"endpoints":[{"scheme":"http","host":"127.0.0.1","port":9080}]},
               "plugins":{"items":[{"plugin_ref":"gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.noop.v1","config":{}}]}}"#,
        );
        let err = s.create_upstream(&caller(), auth_in_chain).await.err().unwrap();
        assert_status(err.clone(), 400);
        assert!(err.detail().contains("is an auth plugin"));

        // Unknown transform binding is a 400.
        let unknown = upstream_input(
            r#"{"alias":"bad2","protocol":"http",
               "server":{"endpoints":[{"scheme":"http","host":"127.0.0.1","port":9080}]},
               "plugins":{"items":[{"plugin_ref":"gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.nope.v1","config":{}}]}}"#,
        );
        let err = s.create_upstream(&caller(), unknown).await.err().unwrap();
        assert_status(err.clone(), 400);
        assert!(err.detail().contains("unknown transform plugin"));

        // Guard + transform bindings are accepted at upstream level.
        let ok = upstream_input(
            r#"{"alias":"good","protocol":"http",
               "server":{"endpoints":[{"scheme":"http","host":"127.0.0.1","port":9080}]},
               "plugins":{"items":[
                   {"plugin_ref":"gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1","config":{"required_request_headers":"X-Must"}},
                   {"plugin_ref":"gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.request_id.v1","config":{}}
               ]}}"#,
        );
        let up = s.create_upstream(&caller(), ok).await.unwrap();
        assert_eq!(up.plugins.items.len(), 2);
    }

    #[tokio::test]
    async fn route_validation_covers_upstream_ownership_and_plugins() {
        let s = svc();
        // Unknown upstream -> 400.
        let ghost = Uuid::parse_str("bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb").unwrap();
        let err = s.create_route(&caller(), route_input(ghost, "/api")).await.err().unwrap();
        assert_status(err.clone(), 400);
        assert!(err.detail().contains("upstream not owned"));

        // Other tenant's upstream -> 400.
        let up = s.create_upstream(&caller(), http_upstream("own")).await.unwrap();
        let other = Caller {
            tenant_id: Uuid::parse_str("cccccccc-cccc-4ccc-8ccc-cccccccccccc").unwrap(),
            ..caller()
        };
        let err = s.create_route(&other, route_input(up.id, "/api")).await.err().unwrap();
        assert_status(err.clone(), 400);
        assert!(err.detail().contains("upstream not owned"));

        // Empty methods -> 400.
        let bad_route: RouteInput = serde_json::from_value(serde_json::json!({
            "upstream_id": up.id,
            "match": {"http": {"methods": [], "path": "/api"}},
        }))
        .unwrap();
        let err = s.create_route(&caller(), bad_route).await.err().unwrap();
        assert_status(err.clone(), 400);
        assert!(err.detail().contains("at least one method"));

        // Auth plugin in a route chain -> 400.
        let bad_chain: RouteInput = serde_json::from_value(serde_json::json!({
            "upstream_id": up.id,
            "match": {"http": {"methods": ["GET"], "path": "/api"}},
            "plugins": {"items": [{"plugin_ref": "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.noop.v1","config":{}}]},
        }))
        .unwrap();
        let err = s.create_route(&caller(), bad_chain).await.err().unwrap();
        assert_status(err.clone(), 400);
        assert!(err.detail().contains("is an auth plugin"));

        // Happy path route with a guard binding.
        let good: RouteInput = serde_json::from_value(serde_json::json!({
            "upstream_id": up.id,
            "match": {"http": {"methods": ["GET"], "path": "/api"}},
            "plugins": {"items": [{"plugin_ref": "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1","config":{"required_request_headers":"X-Route"}}]},
        }))
        .unwrap();
        let route = s.create_route(&caller(), good).await.unwrap();
        assert_eq!(route.plugins.items.len(), 1);

        // upstream_id is immutable on update.
        let other_up = s.create_upstream(&caller(), http_upstream("other")).await.unwrap();
        let mut input = route_input(other_up.id, "/api");
        input.match_config = route.match_config.clone();
        let err = s.update_route(&caller(), route.id, input).await.err().unwrap();
        assert_status(err.clone(), 400);
        assert!(err.detail().contains("immutable"));
    }

    #[tokio::test]
    async fn alias_and_route_resolution() {
        let s = svc();
        let up = s.create_upstream(&caller(), http_upstream("api")).await.unwrap();
        let route = s.create_route(&caller(), route_input(up.id, "/v1")).await.unwrap();

        let res = s.resolve_alias(&caller(), "api").await.unwrap();
        assert_eq!(res.upstream.id, up.id);
        assert!(res.ancestors.is_empty());

        // Unknown alias -> 404.
        let err = s.resolve_alias(&caller(), "nope").await.err().unwrap();
        assert_status(err.clone(), 404);

        // Disabled upstream -> LinkUnavailable (503).
        let mut off_input = http_upstream("off");
        off_input.enabled = false;
        let off = s.create_upstream(&caller(), off_input).await.unwrap();
        let err = s.resolve_alias(&caller(), "off").await.err().unwrap();
        assert_status(err.clone(), 503);
        assert!(matches!(err, OagwError::LinkUnavailable { .. }));
        let _ = off;

        // Route matching: prefix + method.
        let rr = s
            .resolve_route(&caller(), &res, "GET", "/v1/users", &[])
            .await
            .unwrap()
            .expect("route should match /v1/users");
        assert_eq!(rr.route.id, route.id);
        assert_eq!(rr.path_suffix, "users");

        // Method not allowed -> no route.
        assert!(s.resolve_route(&caller(), &res, "POST", "/v1/things", &[]).await.unwrap().is_none());

        // Non-matching path -> no route.
        assert!(s.resolve_route(&caller(), &res, "GET", "/other", &[]).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn custom_plugin_crud_and_reference_checks() {
        let s = svc();
        let input: PluginInput = serde_json::from_value(serde_json::json!({
            "name": "my-guard",
            "type": "guard",
            "source_code": "def handler(ctx): pass",
        }))
        .unwrap();
        let plugin = s.create_plugin(&caller(), input).await.unwrap();
        let gts = format!("{}~{}", plugin.plugin_type.resource_type(), plugin.id);
        assert_eq!(gts, "gts.cf.core.oagw.guard_plugin.v1~".to_owned() + &plugin.id.to_string());

        let got = s.get_plugin(&caller(), plugin.id).await.unwrap();
        assert_eq!(got.id, plugin.id);
        let (items, total) = s
            .list_plugins(&caller(), &ListOptions::default())
            .await
            .unwrap();
        assert_eq!(total, 1);
        assert_eq!(items.len(), 1);
        assert_eq!(
            s.get_plugin_source(&caller(), plugin.id).await.unwrap().as_deref(),
            Some("def handler(ctx): pass")
        );

        // Referencing the custom plugin by GTS id in a chain is accepted
        // (execution fails at data-plane time — no Starlark runtime).
        let up = upstream_input(&format!(
            r#"{{"alias":"cust","protocol":"http",
               "server":{{"endpoints":[{{"scheme":"http","host":"127.0.0.1","port":9080}}]}},
               "plugins":{{"items":[{{"plugin_ref":"{}","config":{{}}}}]}}}}"#,
            gts
        ));
        let up = s.create_upstream(&caller(), up).await.unwrap();
        assert_eq!(up.plugins.items.len(), 1);

        // Deleting a plugin still referenced by an upstream is a 409.
        let err = s.delete_plugin(&caller(), plugin.id).await.err().unwrap();
        assert_status(err.clone(), 409);
        assert!(err.detail().contains("in use"));

        // Unbind (delete the upstream) then the plugin can be deleted.
        s.delete_upstream(&caller(), up.id).await.unwrap();
        s.delete_plugin(&caller(), plugin.id).await.unwrap();
        assert_status(s.get_plugin(&caller(), plugin.id).await.err().unwrap(), 404);
    }
}
