//! Control-plane service: CRUD + validation for upstreams, routes and custom
//! plugins.  Enforces alias derivation/uniqueness/immutability, route
//! upstream references, and plugin-in-use deletion conflicts (ADR-0001).

use std::sync::Arc;

use uuid::Uuid;

use toolkit_security::SecurityContext;

use super::alias::{compute_derived_alias, is_valid_alias_input, is_valid_host, normalize_alias};
use super::dto::{
    AuthConfig, AuthDto, CorsConfig, CorsDto, CustomPlugin, Endpoint, EndpointDto, GrpcMatch,
    HeadersConfig, HeadersDto, HttpMatch, HttpMethod, MatchConfig, MatchDto, PluginBinding,
    PluginRequest, PluginsConfig, PluginsDto, RateLimitConfig, RateLimitDto, RequestHeadersConfig,
    ResponseHeadersConfig, Route, RouteRequest, ServerConfig, Sharing, Upstream, UpstreamRequest,
};
use super::error::{self, DomainError, PERM_UPSTREAM_BIND};
use super::hierarchy::TenantHierarchy;
use super::repo::{DeleteOutcome, OagwRepository};

/// GTS base identifiers used to render `referenced_by` entries.
const UPSTREAM_GTS_BASE: &str = "gts.cf.core.oagw.upstream.v1~";
const ROUTE_GTS_BASE: &str = "gts.cf.core.oagw.route.v1~";

/// How a same-alias ancestor upstream constrains a descendant bind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BindKind {
    /// No sharing-bearing field is visible (all `private`) — the ancestor is
    /// invisible, so a same-alias child is independent (no bind).
    Invisible,
    /// At least one field is `inherit` — the ancestor is visible; binding
    /// requires `oagw:upstream:bind` and the child may override.
    InheritVisible,
    /// At least one field is `enforce` — binding requires the permission and
    /// any differing override is rejected (400).
    EnforceBlocksOverride,
}

/// Classify an ancestor upstream by its sharing-bearing fields
/// (auth / plugins / rate-limit / CORS).  `private` everywhere ⇒ invisible.
fn bind_kind(up: &Upstream) -> BindKind {
    let fields = [
        Some(up.auth.sharing),
        Some(up.plugins.sharing),
        up.rate_limit.as_ref().map(|r| r.sharing),
        up.cors.as_ref().map(|c| c.sharing),
    ];
    let has_enforce = fields.contains(&Some(Sharing::Enforce));
    let has_visible = fields
        .iter()
        .any(|s| *s == Some(Sharing::Enforce) || *s == Some(Sharing::Inherit));
    if has_enforce {
        BindKind::EnforceBlocksOverride
    } else if has_visible {
        BindKind::InheritVisible
    } else {
        BindKind::Invisible
    }
}

/// Whether the two upstreams' shared configuration differs (the fields a bind
/// would override: endpoints, auth, headers, plugins, rate limit, CORS).
fn shared_config_differs(a: &Upstream, b: &Upstream) -> bool {
    a.server != b.server
        || a.auth != b.auth
        || a.headers != b.headers
        || a.plugins != b.plugins
        || a.rate_limit != b.rate_limit
        || a.cors != b.cors
}

/// Scheme values accepted at the management API.  `http`/`ws` are only
/// accepted when the gateway is configured with `allow_http_upstream`.
fn is_allowed_scheme(scheme: &str, allow_http_upstream: bool) -> bool {
    match scheme {
        "https" | "wss" | "wt" | "grpc" => true,
        "http" | "ws" => allow_http_upstream,
        _ => false,
    }
}

fn validate_endpoint(dto: &EndpointDto, allow_http_upstream: bool) -> Result<(), DomainError> {
    let scheme = dto.scheme.to_ascii_lowercase();
    if !is_allowed_scheme(&scheme, allow_http_upstream) {
        return Err(DomainError::Validation(format!(
            "server.endpoints[].scheme '{}' is not allowed",
            dto.scheme
        )));
    }
    if !is_valid_host(&dto.host) {
        return Err(DomainError::Validation(format!(
            "server.endpoints[].host '{}' is not a valid hostname or IP",
            dto.host
        )));
    }
    Ok(())
}

/// Convert wire DTOs into normalized (stored) entity config.
#[must_use]
pub fn auth_dto_to_config(d: &AuthDto) -> AuthConfig {
    AuthConfig {
        plugin_type: d.r#type.clone(),
        sharing: d.sharing,
        config: d.config.clone(),
    }
}

#[must_use]
pub fn headers_dto_to_config(d: &HeadersDto) -> HeadersConfig {
    HeadersConfig {
        request: RequestHeadersConfig {
            set: d.request.set.clone(),
            add: d.request.add.clone(),
            remove: d.request.remove.clone(),
            passthrough: d.request.passthrough,
            passthrough_allowlist: d.request.passthrough_allowlist.clone(),
        },
        response: ResponseHeadersConfig {
            set: d.response.set.clone(),
            add: d.response.add.clone(),
            remove: d.response.remove.clone(),
        },
    }
}

#[must_use]
pub fn plugins_dto_to_config(d: &PluginsDto) -> PluginsConfig {
    PluginsConfig {
        sharing: d.sharing,
        items: d
            .items
            .iter()
            .map(|item| PluginBinding {
                plugin_ref: item.plugin_ref().to_owned(),
                config: item.config(),
            })
            .collect(),
    }
}

/// Normalize a rate-limit DTO into the stored config.
///
/// # Errors
///
/// Returns `DomainError::Validation` when the DTO fails its own validation
/// (zero sustained rate or burst capacity).
pub fn ratelimit_dto_to_config(d: &RateLimitDto) -> Result<RateLimitConfig, DomainError> {
    d.validate().map_err(DomainError::Validation)?;
    Ok(RateLimitConfig {
        sharing: d.sharing,
        algorithm: d.algorithm,
        sustained_rate: d.sustained.rate,
        sustained_window: d.sustained.window,
        burst_capacity: d.burst.capacity.unwrap_or(d.sustained.rate),
        scope: d.scope,
        strategy: d.strategy,
        cost: d.cost,
        response_headers: d.response_headers,
    })
}

/// Normalize a CORS DTO into the stored config.
///
/// # Errors
///
/// Returns `DomainError::Validation` when the DTO fails its own validation
/// (`allow_credentials` combined with the wildcard origin).
pub fn cors_dto_to_config(d: &CorsDto) -> Result<CorsConfig, DomainError> {
    d.validate().map_err(DomainError::Validation)?;
    Ok(CorsConfig {
        sharing: d.sharing,
        enabled: d.enabled,
        allowed_origins: d.allowed_origins.clone(),
        allowed_methods: d.allowed_methods.clone(),
        expose_headers: d.expose_headers.clone(),
        allow_credentials: d.allow_credentials,
    })
}

#[must_use]
pub fn match_dto_to_config(d: &MatchDto) -> MatchConfig {
    MatchConfig {
        http: d.http.as_ref().map(|h| HttpMatch {
            methods: h.methods.clone(),
            path: h.path.clone(),
            query_allowlist: h.query_allowlist.clone(),
            path_suffix_mode: h.path_suffix_mode,
        }),
        grpc: d.grpc.as_ref().map(|g| GrpcMatch {
            service: g.service.clone(),
            method: g.method.clone(),
        }),
    }
}

/// In-memory backed control-plane service.  Sync methods keep the repository
/// trait synchronous (the in-process store is guarded by a `RwLock`).  Alias
/// bind + ancestor-enablement checks walk the (async) tenant hierarchy, so
/// the create/update upstream operations are async.
pub struct ControlPlaneService<R> {
    repo: Arc<R>,
    hierarchy: Arc<dyn TenantHierarchy>,
    allow_http_upstream: bool,
}

impl<R: OagwRepository> ControlPlaneService<R> {
    pub fn new(
        repo: Arc<R>,
        hierarchy: Arc<dyn TenantHierarchy>,
        allow_http_upstream: bool,
    ) -> Self {
        Self {
            repo,
            hierarchy,
            allow_http_upstream,
        }
    }

    fn now_secs() -> i64 {
        // Epoch seconds fit comfortably in i64 for centuries to come; clamp
        // rather than truncate on the (impossible) overflow.
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
    }

    /// Enforce management-side ancestor constraints for `(tenant, alias)`:
    ///
    /// * a matching ancestor that is *disabled* blocks shadowing
    ///   (PRD enable/disable: descendants must not re-enable an
    ///   ancestor-disabled resource),
    /// * a *private*-shared ancestor is invisible (no bind; child
    ///   independent),
    /// * a visible ancestor makes the operation a "bind" requiring
    ///   `oagw:upstream:bind` (DESIGN CRUD semantics),
    /// * an `enforce`-shared ancestor rejects a differing override (400).
    ///
    /// `prospective` is the exact upstream that would be stored; used to
    /// detect overrides against an enforce ancestor.
    async fn enforce_ancestor_rules(
        &self,
        ctx: &SecurityContext,
        tenant_id: Uuid,
        alias: &str,
        prospective: &Upstream,
    ) -> Result<(), DomainError> {
        let chain = self.hierarchy.ancestor_chain(ctx, tenant_id).await;
        // chain is ROOT → LEAF; the leaf is the tenant itself.
        for ancestor in chain.iter().take(chain.len().saturating_sub(1)) {
            let Some(anc_up) = self.repo.find_upstream_by_alias(*ancestor, alias) else {
                continue;
            };
            // Disabled ancestors are absolute blockers (PRD:216).
            if !anc_up.enabled {
                return Err(DomainError::Validation(format!(
                    "alias '{alias}' matches an ancestor upstream that is disabled; \
                     descendants cannot re-enable an ancestor-disabled resource"
                )));
            }
            match bind_kind(&anc_up) {
                BindKind::Invisible => {} // an invisible ancestor imposes no bind
                BindKind::EnforceBlocksOverride => {
                    if !error::scope_allows(ctx.token_scopes(), PERM_UPSTREAM_BIND) {
                        return Err(DomainError::PermissionDenied(format!(
                            "binding ancestor alias '{alias}' requires {PERM_UPSTREAM_BIND}"
                        )));
                    }
                    if shared_config_differs(&anc_up, prospective) {
                        return Err(DomainError::Validation(format!(
                            "alias '{alias}' is enforced by an ancestor upstream; \
                             a differing override is not allowed (delete and recreate \
                             under the ancestor instead)"
                        )));
                    }
                }
                BindKind::InheritVisible => {
                    if !error::scope_allows(ctx.token_scopes(), PERM_UPSTREAM_BIND) {
                        return Err(DomainError::PermissionDenied(format!(
                            "binding ancestor alias '{alias}' requires {PERM_UPSTREAM_BIND}"
                        )));
                    }
                }
            }
        }
        Ok(())
    }

    // ---- upstreams --------------------------------------------------------

    /// Derive or validate the alias, and ensure uniqueness within the tenant.
    fn resolve_alias(
        &self,
        tenant_id: Uuid,
        explicit: Option<&str>,
        endpoints: &[Endpoint],
        current_id: Option<Uuid>,
    ) -> Result<String, DomainError> {
        let alias = if let Some(alias) = explicit {
            let normalized = normalize_alias(alias);
            if !is_valid_alias_input(&normalized) {
                return Err(DomainError::Validation(format!(
                    "alias '{alias}' is not a valid alias (use [a-z0-9:.-])"
                )));
            }
            normalized
        } else {
            let tuples: Vec<(String, String, u16)> = endpoints
                .iter()
                .map(|e| (e.scheme.clone(), e.host.clone(), e.port))
                .collect();
            compute_derived_alias(&tuples).ok_or_else(|| {
                DomainError::Validation(
                    "alias is required: this endpoint pool cannot be auto-derived \
                     (IP endpoints, mixed ports or non-registrable common suffix)"
                        .into(),
                )
            })?
        };

        // Uniqueness within the tenant scope (descendants may shadow ancestors
        // — cross-tenant shadows are the documented inheritance mechanism).
        if let Some(existing) = self.repo.find_upstream_by_alias(tenant_id, &alias)
            && current_id != Some(existing.id)
        {
            return Err(DomainError::AliasConflict(format!(
                "alias '{alias}' is already used by another upstream"
            )));
        }
        Ok(alias)
    }

    /// Create an upstream, deriving/validating its alias and consulting the
    /// tenant hierarchy for ancestor bind/enablement constraints.
    ///
    /// # Errors
    ///
    /// Fails with `DomainError::Validation` (bad endpoints, alias or ancestor
    /// override), `DomainError::AliasConflict` (duplicate alias in the
    /// tenant), or `DomainError::PermissionDenied` (missing `oagw:upstream:bind`
    /// for a bind against a visible ancestor).
    pub async fn create_upstream(
        &self,
        ctx: &SecurityContext,
        tenant_id: Uuid,
        req: &UpstreamRequest,
    ) -> Result<Upstream, DomainError> {
        if req.server.endpoints.is_empty() {
            return Err(DomainError::Validation(
                "server.endpoints must not be empty".into(),
            ));
        }
        for ep in &req.server.endpoints {
            validate_endpoint(ep, self.allow_http_upstream)?;
        }
        if let Some(rl) = &req.rate_limit {
            rl.validate().map_err(DomainError::Validation)?;
        }
        if let Some(cors) = &req.cors {
            cors.validate().map_err(DomainError::Validation)?;
        }

        let endpoints: Vec<Endpoint> = req
            .server
            .endpoints
            .iter()
            .cloned()
            .map(Endpoint::from)
            .collect();
        let alias = self.resolve_alias(tenant_id, req.alias.as_deref(), &endpoints, None)?;

        let upstream = Upstream {
            id: Uuid::new_v4(),
            tenant_id,
            enabled: req.enabled,
            alias,
            tags: req.tags.clone(),
            server: ServerConfig { endpoints },
            protocol: req.protocol,
            auth: auth_dto_to_config(&req.auth),
            headers: headers_dto_to_config(&req.headers),
            plugins: plugins_dto_to_config(&req.plugins),
            rate_limit: req
                .rate_limit
                .as_ref()
                .map(ratelimit_dto_to_config)
                .transpose()?,
            cors: req.cors.as_ref().map(cors_dto_to_config).transpose()?,
            created_at: Self::now_secs(),
        };
        self.enforce_ancestor_rules(ctx, tenant_id, &upstream.alias, &upstream)
            .await?;
        self.repo.insert_upstream(upstream.clone());
        Ok(upstream)
    }

    /// Register a fully built upstream (used by tests / seeders).
    ///
    /// # Errors
    ///
    /// Returns `DomainError::AliasConflict` when an upstream with the same
    /// alias already exists in the tenant.
    pub fn insert_upstream(&self, upstream: Upstream) -> Result<(), DomainError> {
        if self
            .repo
            .find_upstream_by_alias(upstream.tenant_id, &upstream.alias)
            .is_some()
        {
            return Err(DomainError::AliasConflict(format!(
                "alias '{}' is already used by another upstream",
                upstream.alias
            )));
        }
        self.repo.insert_upstream(upstream);
        Ok(())
    }

    /// Fetch an upstream by alias.
    ///
    /// # Errors
    ///
    /// Returns `DomainError::NotFound` when no upstream in the tenant has the
    /// given alias.
    pub fn get_upstream(&self, tenant_id: Uuid, alias: &str) -> Result<Upstream, DomainError> {
        self.repo
            .find_upstream_by_alias(tenant_id, &normalize_alias(alias))
            .ok_or_else(|| DomainError::NotFound(format!("upstream '{alias}' not found")))
    }

    /// Fetch an upstream by id.
    ///
    /// # Errors
    ///
    /// Returns `DomainError::NotFound` when no upstream in the tenant has the
    /// given id.
    pub fn get_upstream_by_id(&self, tenant_id: Uuid, id: Uuid) -> Result<Upstream, DomainError> {
        self.repo
            .get_upstream(tenant_id, id)
            .ok_or_else(|| DomainError::NotFound(format!("upstream '{id}' not found")))
    }

    #[must_use]
    pub fn list_upstreams(&self, tenant_id: Uuid) -> Vec<Upstream> {
        self.repo.list_upstreams(tenant_id)
    }

    /// Replace an upstream by alias.  The alias itself is immutable: the body
    /// alias (if present) must equal the path alias, and the stored alias is
    /// preserved.  Endpoint changes must not alter the derived alias (DESIGN
    /// "Alias Update Behavior" transition table); ancestor bind/enablement
    /// constraints are re-validated.
    /// Replace an upstream by alias (alias immutable; alias update-transition
    /// rules apply and ancestor bind/enablement constraints are re-validated).
    ///
    /// # Errors
    ///
    /// Fails with `DomainError::NotFound` (unknown alias), `DomainError::Validation`
    /// (immutable-alias change, forbidden endpoint transition, disabled/forced
    /// ancestor constraint), or `DomainError::PermissionDenied` (missing
    /// `oagw:upstream:bind` for a visible-ancestor bind).
    pub async fn replace_upstream(
        &self,
        ctx: &SecurityContext,
        tenant_id: Uuid,
        alias: &str,
        req: &UpstreamRequest,
    ) -> Result<Upstream, DomainError> {
        let existing = self.get_upstream(tenant_id, alias)?;
        let path_alias = normalize_alias(alias);
        if let Some(body_alias) = req.alias.as_deref()
            && normalize_alias(body_alias) != path_alias
        {
            return Err(DomainError::Validation(format!(
                "alias is immutable: '{body_alias}' cannot replace '{path_alias}' \
                 (delete and recreate to change the alias)"
            )));
        }

        for ep in &req.server.endpoints {
            validate_endpoint(ep, self.allow_http_upstream)?;
        }
        if let Some(rl) = &req.rate_limit {
            rl.validate().map_err(DomainError::Validation)?;
        }
        if let Some(cors) = &req.cors {
            cors.validate().map_err(DomainError::Validation)?;
        }

        let endpoints: Vec<Endpoint> = req
            .server
            .endpoints
            .iter()
            .cloned()
            .map(Endpoint::from)
            .collect();

        // Alias update-transition rules: the derived alias is recomputed from
        // the new endpoints and must equal the existing (immutable) alias for
        // any derivable target; a hostname→IP (derivable→non-derivable)
        // transition is always rejected.
        let old_derived = compute_derived_alias(
            &existing
                .server
                .endpoints
                .iter()
                .map(|e| (e.scheme.clone(), e.host.clone(), e.port))
                .collect::<Vec<_>>(),
        );
        let new_derived = compute_derived_alias(
            &endpoints
                .iter()
                .map(|e| (e.scheme.clone(), e.host.clone(), e.port))
                .collect::<Vec<_>>(),
        );
        match (old_derived, new_derived) {
            // Non-derivable → non-derivable (IP → IP): existing alias retained.
            (None, None) => {}
            // Derivable → non-derivable (hostname → IP): always rejected, even
            // with an explicit alias.
            (Some(_old), None) => {
                return Err(DomainError::Validation(format!(
                    "endpoint change would make alias '{path_alias}' non-derivable \
                     (hostname → IP transition is not allowed); delete and recreate \
                     the upstream instead"
                )));
            }
            // Non-derivable → derivable (IP → hostname): the recomputed alias
            // must equal the existing (explicit-IP) alias, otherwise rejected.
            (None, Some(derived)) => {
                if derived != path_alias {
                    return Err(DomainError::Validation(format!(
                        "endpoint change would derive alias '{derived}' which differs \
                         from existing alias '{path_alias}'; delete and recreate to \
                         change the alias"
                    )));
                }
            }
            // Derivable → derivable: recomputed alias must equal existing.
            (Some(_old), Some(new)) => {
                if new != path_alias {
                    return Err(DomainError::Validation(format!(
                        "endpoint change would change the derived alias from \
                         '{path_alias}' to '{new}'; delete and recreate to change the alias"
                    )));
                }
            }
        }

        // Alias preserved; endpoints may change (they will be resolved by the
        // data plane on each request).
        let replaced = Upstream {
            id: existing.id,
            tenant_id,
            enabled: req.enabled,
            alias: path_alias.clone(),
            tags: req.tags.clone(),
            server: ServerConfig { endpoints },
            protocol: req.protocol,
            auth: auth_dto_to_config(&req.auth),
            headers: headers_dto_to_config(&req.headers),
            plugins: plugins_dto_to_config(&req.plugins),
            rate_limit: req
                .rate_limit
                .as_ref()
                .map(ratelimit_dto_to_config)
                .transpose()?,
            cors: req.cors.as_ref().map(cors_dto_to_config).transpose()?,
            created_at: existing.created_at,
        };
        self.enforce_ancestor_rules(ctx, tenant_id, &path_alias, &replaced)
            .await?;
        self.repo.update_upstream(replaced.clone());
        Ok(replaced)
    }

    /// Delete an upstream by alias, blocked while routes reference it.
    ///
    /// # Errors
    ///
    /// Fails with `DomainError::NotFound` (unknown alias) or
    /// `DomainError::Validation` when routes still reference the upstream.
    pub fn delete_upstream(
        &self,
        tenant_id: Uuid,
        alias: &str,
    ) -> Result<DeleteOutcome, DomainError> {
        let upstream = self.get_upstream(tenant_id, alias)?;
        let routes = self.repo.list_routes_for_upstream(tenant_id, upstream.id);
        if !routes.is_empty() {
            return Err(DomainError::Validation(format!(
                "upstream '{}' is referenced by {} route(s); delete the routes first",
                upstream.alias,
                routes.len()
            )));
        }
        Ok(self.repo.remove_upstream(tenant_id, upstream.id))
    }

    // ---- routes -----------------------------------------------------------

    /// Whether two routes carry a conflicting HTTP match rule: same path,
    /// same selection priority and at least one shared method (DESIGN:826).
    fn http_match_conflicts(a: &Route, b: &Route) -> bool {
        let (Some(ha), Some(hb)) = (&a.match_config.http, &b.match_config.http) else {
            return false;
        };
        if ha.path != hb.path || a.priority != b.priority {
            return false;
        }
        ha.methods.iter().any(|m| hb.methods.contains(m))
    }

    /// Reject a route whose HTTP match collides with an existing route of the
    /// same upstream (same `(path, priority, method)` → 409 `RouteConflict`).
    fn enforce_route_match_uniqueness(
        &self,
        tenant_id: Uuid,
        upstream_id: Uuid,
        route: &Route,
        exclude_id: Option<Uuid>,
    ) -> Result<(), DomainError> {
        for existing in self.repo.list_routes_for_upstream(tenant_id, upstream_id) {
            if Some(existing.id) == exclude_id {
                continue;
            }
            if Self::http_match_conflicts(&existing, route) {
                let methods = route
                    .match_config
                    .http
                    .as_ref()
                    .map(|h| {
                        h.methods
                            .iter()
                            .map(super::dto::HttpMethod::as_str)
                            .collect::<Vec<_>>()
                            .join(",")
                    })
                    .unwrap_or_default();
                return Err(DomainError::RouteConflict(format!(
                    "route {} (priority {}) already matches path '{}' for method(s) [{}] \
                     under upstream {upstream_id}",
                    existing.id,
                    existing.priority,
                    route
                        .match_config
                        .http
                        .as_ref()
                        .map_or("", |h| h.path.as_str()),
                    methods
                )));
            }
        }
        Ok(())
    }

    /// Create a route bound to an upstream in the tenant scope, enforcing
    /// match-rule uniqueness.
    ///
    /// # Errors
    ///
    /// Fails with `DomainError::Validation` (unknown upstream, empty match,
    /// duplicate methods or invalid rate-limit/CORS) or
    /// `DomainError::RouteConflict` on a duplicate match rule.
    pub fn create_route(&self, tenant_id: Uuid, req: &RouteRequest) -> Result<Route, DomainError> {
        // The referenced upstream must exist in this tenant scope.
        self.get_upstream_by_id(tenant_id, req.upstream_id)
            .map_err(|_| {
                DomainError::Validation(format!(
                    "upstream_id '{}' does not exist in this tenant scope",
                    req.upstream_id
                ))
            })?;

        if req.match_config.http.is_none() && req.match_config.grpc.is_none() {
            return Err(DomainError::Validation(
                "match must contain http or grpc".into(),
            ));
        }
        if let Some(http) = &req.match_config.http {
            let mut seen: Vec<&HttpMethod> = Vec::new();
            for m in &http.methods {
                if seen.contains(&m) {
                    return Err(DomainError::Validation(format!(
                        "match.http.methods contains duplicate method '{}'",
                        m.as_str()
                    )));
                }
                seen.push(m);
            }
        }
        if let Some(rl) = &req.rate_limit {
            rl.validate().map_err(DomainError::Validation)?;
        }
        if let Some(cors) = &req.cors {
            cors.validate().map_err(DomainError::Validation)?;
        }

        let route = Route {
            id: Uuid::new_v4(),
            tenant_id,
            enabled: req.enabled,
            upstream_id: req.upstream_id,
            priority: req.priority,
            match_config: match_dto_to_config(&req.match_config),
            tags: req.tags.clone(),
            plugins: plugins_dto_to_config(&req.plugins),
            rate_limit: req
                .rate_limit
                .as_ref()
                .map(ratelimit_dto_to_config)
                .transpose()?,
            cors: req.cors.as_ref().map(cors_dto_to_config).transpose()?,
            created_at: Self::now_secs(),
        };
        self.enforce_route_match_uniqueness(tenant_id, req.upstream_id, &route, None)?;
        self.repo.insert_route(route.clone());
        Ok(route)
    }

    /// Fetch a route by id.
    ///
    /// # Errors
    ///
    /// Returns `DomainError::NotFound` when no route in the tenant has the
    /// given id.
    pub fn get_route(&self, tenant_id: Uuid, id: Uuid) -> Result<Route, DomainError> {
        self.repo
            .get_route(tenant_id, id)
            .ok_or_else(|| DomainError::NotFound(format!("route '{id}' not found")))
    }

    #[must_use]
    pub fn list_routes(&self, tenant_id: Uuid) -> Vec<Route> {
        self.repo.list_routes(tenant_id)
    }

    /// Replace a route, re-validating the upstream reference and match-rule
    /// uniqueness (excluding the route itself).
    ///
    /// # Errors
    ///
    /// Fails with `DomainError::NotFound` (unknown route or upstream) or
    /// `DomainError::RouteConflict` when the replacement collides with another
    /// route's match rule.
    pub fn replace_route(
        &self,
        tenant_id: Uuid,
        id: Uuid,
        req: &RouteRequest,
    ) -> Result<Route, DomainError> {
        let existing = self.get_route(tenant_id, id)?;
        self.get_upstream_by_id(tenant_id, req.upstream_id)
            .map_err(|_| {
                DomainError::Validation(format!(
                    "upstream_id '{}' does not exist in this tenant scope",
                    req.upstream_id
                ))
            })?;
        if req.match_config.http.is_none() && req.match_config.grpc.is_none() {
            return Err(DomainError::Validation(
                "match must contain http or grpc".into(),
            ));
        }
        if let Some(rl) = &req.rate_limit {
            rl.validate().map_err(DomainError::Validation)?;
        }
        if let Some(cors) = &req.cors {
            cors.validate().map_err(DomainError::Validation)?;
        }
        let replaced = Route {
            id: existing.id,
            tenant_id,
            enabled: req.enabled,
            upstream_id: req.upstream_id,
            priority: req.priority,
            match_config: match_dto_to_config(&req.match_config),
            tags: req.tags.clone(),
            plugins: plugins_dto_to_config(&req.plugins),
            rate_limit: req
                .rate_limit
                .as_ref()
                .map(ratelimit_dto_to_config)
                .transpose()?,
            cors: req.cors.as_ref().map(cors_dto_to_config).transpose()?,
            created_at: existing.created_at,
        };
        self.enforce_route_match_uniqueness(
            tenant_id,
            existing.upstream_id,
            &replaced,
            Some(existing.id),
        )?;
        self.repo.update_route(replaced.clone());
        Ok(replaced)
    }

    /// Delete a route.
    ///
    /// # Errors
    ///
    /// This is an infallible deletion; the `Result` wrapper keeps the handler
    /// shift uniform with every other delete in the control plane.
    // The `Result` wrapper is load-bearing for the uniform 204/404 delete
    // handler; removing it would change the public control-plane signature.
    #[allow(clippy::unnecessary_wraps)]
    pub fn delete_route(&self, tenant_id: Uuid, id: Uuid) -> Result<DeleteOutcome, DomainError> {
        Ok(self.repo.remove_route(tenant_id, id))
    }

    // ---- custom plugins ---------------------------------------------------

    /// Create a custom (Starlark) plugin.
    ///
    /// # Errors
    ///
    /// Returns `DomainError::Validation` when `name` or `source_code` is
    /// empty.
    pub fn create_plugin(
        &self,
        tenant_id: Uuid,
        req: &PluginRequest,
    ) -> Result<CustomPlugin, DomainError> {
        if req.name.trim().is_empty() {
            return Err(DomainError::Validation("name must not be empty".into()));
        }
        if req.source_code.trim().is_empty() {
            return Err(DomainError::Validation(
                "source_code must not be empty".into(),
            ));
        }
        let plugin = CustomPlugin {
            id: Uuid::new_v4(),
            tenant_id,
            plugin_type: req.plugin_type,
            name: req.name.clone(),
            description: req.description.clone(),
            config_schema: req.config_schema.clone(),
            source_code: req.source_code.clone(),
            created_at: Self::now_secs(),
        };
        self.repo.insert_plugin(plugin.clone());
        Ok(plugin)
    }

    /// Fetch a custom plugin by id.
    ///
    /// # Errors
    ///
    /// Returns `DomainError::NotFound` when no plugin in the tenant has the
    /// given id.
    pub fn get_plugin(&self, tenant_id: Uuid, id: Uuid) -> Result<CustomPlugin, DomainError> {
        self.repo
            .get_plugin(tenant_id, id)
            .ok_or_else(|| DomainError::NotFound(format!("plugin '{id}' not found")))
    }

    #[must_use]
    pub fn list_plugins(&self, tenant_id: Uuid) -> Vec<CustomPlugin> {
        self.repo.list_plugins(tenant_id)
    }

    /// Delete a plugin, rejecting when any upstream or route in the tenant
    /// scope still binds it (ADR-0001 409 + `referenced_by`).
    ///
    /// # Errors
    ///
    /// Returns `DomainError::NotFound` for an unknown plugin id, or
    /// `DomainError::PluginInUse` when an upstream or route still binds the
    /// plugin's GTS id.
    pub fn delete_plugin(&self, tenant_id: Uuid, id: Uuid) -> Result<DeleteOutcome, DomainError> {
        let plugin = self.get_plugin(tenant_id, id)?;
        let gts_id = plugin.gts_id();

        let mut upstreams: Vec<String> = Vec::new();
        for up in self.repo.list_upstreams(tenant_id) {
            if up.plugins.items.iter().any(|b| b.plugin_ref == gts_id) {
                upstreams.push(format!("{UPSTREAM_GTS_BASE}{}", up.id));
            }
        }
        let mut routes: Vec<String> = Vec::new();
        for route in self.repo.list_routes(tenant_id) {
            if route.plugins.items.iter().any(|b| b.plugin_ref == gts_id) {
                routes.push(format!("{ROUTE_GTS_BASE}{}", route.id));
            }
        }
        if !upstreams.is_empty() || !routes.is_empty() {
            return Err(DomainError::PluginInUse {
                plugin_id: gts_id,
                upstreams,
                routes,
            });
        }
        Ok(self.repo.remove_plugin(tenant_id, id))
    }

    // ---- shared helpers ---------------------------------------------------

    #[must_use]
    pub fn repo(&self) -> &Arc<R> {
        &self.repo
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use crate::domain::hierarchy::{FlatTenantHierarchy, InMemoryHierarchy};
    use crate::domain::repo::OagwRepository;
    use crate::infra::storage::InMemoryRepository;
    use serde_json::{Value, json};
    use std::collections::HashMap;
    use toolkit_security::context::SecurityContextBuilder;

    fn tenant() -> Uuid {
        Uuid::nil()
    }

    // Chain parent(1) -> child(2) -> grandchild(3); distinct uuids per level.
    fn uuid(n: u8) -> Uuid {
        Uuid::from_u128(u128::from(n))
    }

    fn ctx_scopes(scopes: &[&str]) -> SecurityContext {
        SecurityContextBuilder::default()
            .subject_id(tenant())
            .subject_type("user")
            .subject_tenant_id(tenant())
            .token_scopes(scopes.iter().copied().map(str::to_owned).collect())
            .build()
            .unwrap()
    }

    fn allowed_ctx() -> SecurityContext {
        ctx_scopes(&["*"])
    }

    fn up_req() -> UpstreamRequest {
        serde_json::from_value(json!({
            "enabled": true,
            "tags": ["prod"],
            "server": { "endpoints": [
                { "scheme": "https", "host": "api.example.com" }
            ]},
            "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1"
        }))
        .unwrap()
    }

    fn flat_svc() -> ControlPlaneService<InMemoryRepository> {
        let repo = Arc::new(InMemoryRepository::new());
        ControlPlaneService::new(repo, Arc::new(FlatTenantHierarchy), true)
    }

    /// Service whose hierarchy is `root(uuid(1)) <- mid(uuid(2)) <- leaf(uuid(3))`.
    fn chained_svc() -> (
        ControlPlaneService<InMemoryRepository>,
        Arc<InMemoryRepository>,
    ) {
        let repo = Arc::new(InMemoryRepository::new());
        let mut parents = HashMap::new();
        parents.insert(uuid(2), uuid(1));
        parents.insert(uuid(3), uuid(2));
        let hierarchy: Arc<dyn TenantHierarchy> = Arc::new(InMemoryHierarchy::new(parents));
        (
            ControlPlaneService::new(repo.clone(), hierarchy, true),
            repo,
        )
    }

    #[tokio::test]
    async fn create_upstream_derives_alias_from_single_host() {
        let svc = flat_svc();
        let up = svc
            .create_upstream(&allowed_ctx(), tenant(), &up_req())
            .await
            .unwrap();
        assert_eq!(up.alias, "api.example.com");
        assert_eq!(up.server.endpoints[0].port, 443);
    }

    #[tokio::test]
    async fn explicit_alias_is_normalized_and_used() {
        let svc = flat_svc();
        let mut req = up_req();
        req.alias = Some("My-Service.".into());
        let up = svc
            .create_upstream(&allowed_ctx(), tenant(), &req)
            .await
            .unwrap();
        assert_eq!(up.alias, "my-service");
    }

    #[tokio::test]
    async fn duplicate_alias_in_tenant_conflicts() {
        let svc = flat_svc();
        let up = svc
            .create_upstream(&allowed_ctx(), tenant(), &up_req())
            .await
            .unwrap();
        let mut req = up_req();
        req.alias = Some(up.alias.clone());
        let err = svc
            .create_upstream(&allowed_ctx(), tenant(), &req)
            .await
            .unwrap_err();
        assert!(matches!(err, DomainError::AliasConflict(_)));
    }

    #[tokio::test]
    async fn ip_endpoint_requires_explicit_alias() {
        let svc = flat_svc();
        let mut req = up_req();
        req.server.endpoints[0].host = "10.0.1.5".into();
        req.server.endpoints[0].scheme = "http".into();
        req.server.endpoints[0].port = Some(8080);
        let err = svc
            .create_upstream(&allowed_ctx(), tenant(), &req)
            .await
            .unwrap_err();
        assert!(matches!(err, DomainError::Validation(_)));
        req.alias = Some("ip-svc".into());
        let up = svc
            .create_upstream(&allowed_ctx(), tenant(), &req)
            .await
            .unwrap();
        assert_eq!(up.alias, "ip-svc");
    }

    #[tokio::test]
    async fn http_scheme_rejected_when_disabled() {
        let repo = Arc::new(InMemoryRepository::new());
        let svc = ControlPlaneService::<InMemoryRepository>::new(
            repo,
            Arc::new(FlatTenantHierarchy),
            false,
        );
        let mut req = up_req();
        req.server.endpoints[0].scheme = "http".into();
        req.server.endpoints[0].port = Some(80);
        let err = svc
            .create_upstream(&allowed_ctx(), tenant(), &req)
            .await
            .unwrap_err();
        assert!(matches!(err, DomainError::Validation(_)));
    }

    #[tokio::test]
    async fn replace_preserves_alias_and_created_at() {
        let svc = flat_svc();
        let up = svc
            .create_upstream(&allowed_ctx(), tenant(), &up_req())
            .await
            .unwrap();
        let mut req = up_req();
        req.alias = Some("api.example.com".into());
        req.tags = vec!["updated".into()];
        let replaced = svc
            .replace_upstream(&allowed_ctx(), tenant(), &up.alias, &req)
            .await
            .unwrap();
        assert_eq!(replaced.alias, up.alias);
        assert_eq!(replaced.created_at, up.created_at);
        assert_eq!(replaced.tags, vec!["updated"]);
    }

    #[tokio::test]
    async fn replace_rejects_alias_change() {
        let svc = flat_svc();
        let up = svc
            .create_upstream(&allowed_ctx(), tenant(), &up_req())
            .await
            .unwrap();
        let mut req = up_req();
        req.alias = Some("different.example.com".into());
        let err = svc
            .replace_upstream(&allowed_ctx(), tenant(), &up.alias, &req)
            .await
            .unwrap_err();
        assert!(matches!(err, DomainError::Validation(_)));
    }

    // ---- alias update-transition rules (DESIGN "Alias Update Behavior") ---

    #[tokio::test]
    async fn replace_rejects_derived_alias_change() {
        let svc = flat_svc();
        let up = svc
            .create_upstream(&allowed_ctx(), tenant(), &up_req())
            .await
            .unwrap();
        assert_eq!(up.alias, "api.example.com");
        // Same hostname, different (non-standard) port changes the derived
        // alias (`:8443` suffix) → rejected.
        let mut req = up_req();
        req.server.endpoints[0].port = Some(8443);
        let err = svc
            .replace_upstream(&allowed_ctx(), tenant(), &up.alias, &req)
            .await
            .unwrap_err();
        assert!(matches!(err, DomainError::Validation(_)));
        // Different hostname → derived alias changes → rejected.
        let mut req = up_req();
        req.server.endpoints[0].host = "other.example.com".into();
        let err = svc
            .replace_upstream(&allowed_ctx(), tenant(), &up.alias, &req)
            .await
            .unwrap_err();
        assert!(matches!(err, DomainError::Validation(_)));
    }

    #[tokio::test]
    async fn replace_allows_derivable_endpoint_change_keeping_alias() {
        let svc = flat_svc();
        // us.vendor.com + eu.vendor.com → vendor.com.
        let mut pool_req = up_req();
        pool_req.server.endpoints = serde_json::from_value(json!([
            {"scheme": "https", "host": "us.vendor.com"},
            {"scheme": "https", "host": "eu.vendor.com"}
        ]))
        .unwrap();
        let pool = svc
            .create_upstream(&allowed_ctx(), tenant(), &pool_req)
            .await
            .unwrap();
        assert_eq!(pool.alias, "vendor.com");
        // Swap one endpoint for another under the same registrable suffix:
        // vendor.com still derives → allowed.
        let mut updated = pool_req;
        updated.server.endpoints = serde_json::from_value(json!([
            {"scheme": "https", "host": "us.vendor.com"},
            {"scheme": "https", "host": "gb.vendor.com"}
        ]))
        .unwrap();
        updated.alias = Some("vendor.com".into());
        let replaced = svc
            .replace_upstream(&allowed_ctx(), tenant(), &pool.alias, &updated)
            .await
            .unwrap();
        assert_eq!(replaced.alias, "vendor.com");
        assert_eq!(replaced.server.endpoints.len(), 2);
    }

    #[tokio::test]
    async fn replace_rejects_hostname_to_ip_transition_always() {
        let svc = flat_svc();
        let up = svc
            .create_upstream(&allowed_ctx(), tenant(), &up_req())
            .await
            .unwrap();
        // hostname → IP (with explicit alias equal to the path alias): always
        // rejected, even though an explicit alias is provided.
        let mut req = up_req();
        req.server.endpoints[0].scheme = "http".into();
        req.server.endpoints[0].host = "10.0.1.5".into();
        req.server.endpoints[0].port = Some(8080);
        req.alias = Some("api.example.com".into());
        let err = svc
            .replace_upstream(&allowed_ctx(), tenant(), &up.alias, &req)
            .await
            .unwrap_err();
        assert!(matches!(err, DomainError::Validation(_)));
    }

    #[tokio::test]
    async fn replace_retains_alias_for_private_explicit_alias_transition() {
        // Non-derivable → non-derivable (IP → IP): existing alias retained.
        let svc = flat_svc();
        let mut req = up_req();
        req.alias = Some("ip-svc".into());
        req.server.endpoints[0].scheme = "http".into();
        req.server.endpoints[0].host = "10.0.1.5".into();
        req.server.endpoints[0].port = Some(8080);
        let up = svc
            .create_upstream(&allowed_ctx(), tenant(), &req)
            .await
            .unwrap();
        assert_eq!(up.alias, "ip-svc");

        let mut updated = req;
        updated.server.endpoints[0].host = "10.0.1.6".into();
        updated.alias = Some("ip-svc".into());
        let replaced = svc
            .replace_upstream(&allowed_ctx(), tenant(), &up.alias, &updated)
            .await
            .unwrap();
        assert_eq!(replaced.alias, "ip-svc");
        assert_eq!(replaced.server.endpoints[0].host, "10.0.1.6");
    }

    // ---- ancestor bind + disabled-shadowing rules (findings 6b / 15) ------

    #[tokio::test]
    async fn create_rejects_shadowing_a_disabled_ancestor() {
        let (svc, repo) = chained_svc();
        // Ancestor(uuid 1) creates an upstream, then disables it.
        let mut anc_req = up_req();
        anc_req.alias = Some("shared-svc".into());
        let anc = svc
            .create_upstream(&allowed_ctx(), uuid(1), &anc_req)
            .await
            .unwrap();
        let mut disabled = anc;
        disabled.enabled = false;
        repo.update_upstream(disabled);

        // Leaf cannot create a same-alias upstream (would re-enable).
        let mut leaf_req = up_req();
        leaf_req.alias = Some("shared-svc".into());
        let err = svc
            .create_upstream(&allowed_ctx(), uuid(3), &leaf_req)
            .await
            .unwrap_err();
        assert!(matches!(err, DomainError::Validation(_)));
    }

    #[tokio::test]
    async fn bind_requires_upstream_bind_permission_on_visible_ancestor() {
        let (svc, _repo) = chained_svc();
        // Ancestor upstream with an `inherit`-shared auth → visible.
        let mut anc_req = up_req();
        anc_req.alias = Some("shared-svc".into());
        anc_req.auth = serde_json::from_value(json!({
            "type": "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1",
            "sharing": "inherit",
            "config": { "secret_ref": "cred://k" }
        }))
        .unwrap();
        svc.create_upstream(&allowed_ctx(), uuid(1), &anc_req)
            .await
            .unwrap();

        // Caller lacking oagw:upstream:bind → 403 permission denied.
        let no_bind = ctx_scopes(&["gts.cf.core.oagw.upstream.v1~:create"]);
        let mut leaf_req = up_req();
        leaf_req.alias = Some("shared-svc".into());
        let err = svc
            .create_upstream(&no_bind, uuid(3), &leaf_req)
            .await
            .unwrap_err();
        assert!(matches!(err, DomainError::PermissionDenied(_)));

        // With the bind permission → allowed; child is independent.
        let bind_only = ctx_scopes(&[
            "gts.cf.core.oagw.upstream.v1~:create",
            "gts.cf.core.oagw.upstream.v1~:bind",
        ]);
        let created = svc
            .create_upstream(&bind_only, uuid(3), &leaf_req)
            .await
            .unwrap();
        assert_eq!(created.alias, "shared-svc");
        assert_eq!(created.tenant_id, uuid(3));
    }

    #[tokio::test]
    async fn enforce_ancestor_blocks_differing_override() {
        let (svc, _repo) = chained_svc();
        // Ancestor with an `enforce`-shared rate limit → forced on descendants.
        let mut anc_req = up_req();
        anc_req.alias = Some("shared-svc".into());
        anc_req.rate_limit = Some(
            serde_json::from_value(json!({
                "algorithm": "token_bucket",
                "sustained": { "rate": 10, "window": "second" },
                "strategy": "reject",
                "sharing": "enforce"
            }))
            .unwrap(),
        );
        svc.create_upstream(&allowed_ctx(), uuid(1), &anc_req)
            .await
            .unwrap();

        let bind_ctx = ctx_scopes(&["*"]);
        let mut leaf_req = up_req();
        leaf_req.alias = Some("shared-svc".into());
        leaf_req.rate_limit = Some(
            serde_json::from_value(json!({
                "algorithm": "token_bucket",
                "sustained": { "rate": 999, "window": "second" },
                "strategy": "reject",
                "sharing": "inherit"
            }))
            .unwrap(),
        );
        let err = svc
            .create_upstream(&bind_ctx, uuid(3), &leaf_req)
            .await
            .unwrap_err();
        assert!(matches!(err, DomainError::Validation(_)));

        // An exact (non-differing) replica of the ancestor config is allowed.
        let mut same_req = up_req();
        same_req.alias = Some("shared-svc".into());
        same_req.rate_limit = Some(
            serde_json::from_value(json!({
                "algorithm": "token_bucket",
                "sustained": { "rate": 10, "window": "second" },
                "strategy": "reject",
                "sharing": "enforce"
            }))
            .unwrap(),
        );
        assert!(
            svc.create_upstream(&bind_ctx, uuid(3), &same_req)
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn private_ancestor_is_invisible_no_bind() {
        let (svc, _repo) = chained_svc();
        // Fully private ancestor (all default sharing = private).
        let mut anc_req = up_req();
        anc_req.alias = Some("shared-svc".into());
        anc_req.auth = serde_json::from_value(json!({
            "type": "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1",
            "sharing": "private",
            "config": { "secret_ref": "cred://k" }
        }))
        .unwrap();
        svc.create_upstream(&allowed_ctx(), uuid(1), &anc_req)
            .await
            .unwrap();

        // No bind permission needed — the private ancestor is invisible.
        let no_bind = ctx_scopes(&["gts.cf.core.oagw.upstream.v1~:create"]);
        let mut leaf_req = up_req();
        leaf_req.alias = Some("shared-svc".into());
        let created = svc
            .create_upstream(&no_bind, uuid(3), &leaf_req)
            .await
            .unwrap();
        assert_eq!(created.alias, "shared-svc");
    }

    // ---- routes -----------------------------------------------------------

    #[tokio::test]
    async fn delete_upstream_blocked_when_routes_reference_it() {
        let svc = flat_svc();
        let up = svc
            .create_upstream(&allowed_ctx(), tenant(), &up_req())
            .await
            .unwrap();
        let route_req: RouteRequest = serde_json::from_value(json!({
            "upstream_id": up.id,
            "match": { "http": { "methods": ["GET"], "path": "/v1" } }
        }))
        .unwrap();
        svc.create_route(tenant(), &route_req).unwrap();
        let err = svc.delete_upstream(tenant(), &up.alias).unwrap_err();
        assert!(matches!(err, DomainError::Validation(_)));
    }

    #[tokio::test]
    async fn route_requires_existing_upstream() {
        let svc = flat_svc();
        let route_req: RouteRequest = serde_json::from_value(json!({
            "upstream_id": Uuid::new_v4(),
            "match": { "http": { "methods": ["GET"], "path": "/v1" } }
        }))
        .unwrap();
        let err = svc.create_route(tenant(), &route_req).unwrap_err();
        assert!(matches!(err, DomainError::Validation(_)));
    }

    #[tokio::test]
    async fn route_match_rule_uniqueness_within_upstream_conflicts() {
        let svc = flat_svc();
        let up = svc
            .create_upstream(&allowed_ctx(), tenant(), &up_req())
            .await
            .unwrap();
        let base: RouteRequest = serde_json::from_value(json!({
            "upstream_id": up.id,
            "match": { "http": { "methods": ["GET"], "path": "/v1" } }
        }))
        .unwrap();
        svc.create_route(tenant(), &base).unwrap();

        // Same path + method + priority → 409 RouteConflict.
        let dup = base.clone();
        let err = svc.create_route(tenant(), &dup).unwrap_err();
        assert!(matches!(err, DomainError::RouteConflict(_)));

        // Same path, different priority → allowed.
        let mut diff_prio = base.clone();
        diff_prio.priority = 5;
        assert!(svc.create_route(tenant(), &diff_prio).is_ok());

        // Same path + priority, non-overlapping methods → allowed.
        let mut diff_method = base.clone();
        diff_method.match_config.http.as_mut().unwrap().methods = vec![HttpMethod::Post];
        assert!(svc.create_route(tenant(), &diff_method).is_ok());

        // Different path, same priority+method → allowed.
        let mut diff_path = base.clone();
        diff_path.match_config.http.as_mut().unwrap().path = "/other".into();
        assert!(svc.create_route(tenant(), &diff_path).is_ok());

        // Different upstream, same exact match → allowed (uniqueness is
        // scoped to the upstream).
        let mut other_up_req = up_req();
        other_up_req.server.endpoints[0].host = "api2.example.com".into();
        let up2 = svc
            .create_upstream(&allowed_ctx(), tenant(), &other_up_req)
            .await
            .unwrap();
        let mut other_up = base.clone();
        other_up.upstream_id = up2.id;
        assert!(svc.create_route(tenant(), &other_up).is_ok());
    }

    #[tokio::test]
    async fn route_priority_is_additive_with_default_zero() {
        let svc = flat_svc();
        let up = svc
            .create_upstream(&allowed_ctx(), tenant(), &up_req())
            .await
            .unwrap();
        let req: RouteRequest = serde_json::from_value(json!({
            "upstream_id": up.id,
            "match": { "http": { "methods": ["GET"], "path": "/v1" } }
        }))
        .unwrap();
        let route = svc.create_route(tenant(), &req).unwrap();
        assert_eq!(route.priority, 0); // default
        let mut hi = req;
        hi.priority = 7;
        hi.match_config.http.as_mut().unwrap().path = "/v2".into();
        let route = svc.create_route(tenant(), &hi).unwrap();
        assert_eq!(route.priority, 7);
    }

    #[tokio::test]
    async fn replace_route_revalidates_match_uniqueness_excluding_self() {
        let svc = flat_svc();
        let up = svc
            .create_upstream(&allowed_ctx(), tenant(), &up_req())
            .await
            .unwrap();
        let a: RouteRequest = serde_json::from_value(json!({
            "upstream_id": up.id,
            "match": { "http": { "methods": ["GET"], "path": "/a" } }
        }))
        .unwrap();
        let b: RouteRequest = serde_json::from_value(json!({
            "upstream_id": up.id,
            "match": { "http": { "methods": ["GET"], "path": "/b" } }
        }))
        .unwrap();
        let ra = svc.create_route(tenant(), &a).unwrap();
        let rb = svc.create_route(tenant(), &b).unwrap();

        // Replacing ra with ra's own match is a no-op → allowed.
        assert!(svc.replace_route(tenant(), ra.id, &a).is_ok());

        // Replacing rb with a match colliding with ra → 409.
        let mut collide = b;
        collide.match_config.http.as_mut().unwrap().path = "/a".into();
        let err = svc.replace_route(tenant(), rb.id, &collide).unwrap_err();
        assert!(matches!(err, DomainError::RouteConflict(_)));
    }

    #[tokio::test]
    async fn plugin_delete_with_references_returns_409_info() {
        let svc = flat_svc();
        let plugin_req: PluginRequest = serde_json::from_value(json!({
            "name": "my-plugin",
            "plugin_type": "guard",
            "config_schema": {},
            "source_code": "def on_request(ctx): pass"
        }))
        .unwrap();
        let plugin = svc.create_plugin(tenant(), &plugin_req).unwrap();
        let gts = plugin.gts_id();

        let mut up = svc
            .create_upstream(&allowed_ctx(), tenant(), &up_req())
            .await
            .unwrap();
        up.plugins = PluginsConfig {
            sharing: Sharing::Inherit,
            items: vec![PluginBinding {
                plugin_ref: gts.clone(),
                config: Value::Null,
            }],
        };
        svc.repo().insert_upstream(up);

        match svc.delete_plugin(tenant(), plugin.id) {
            Err(DomainError::PluginInUse {
                plugin_id,
                upstreams,
                routes,
            }) => {
                assert_eq!(plugin_id, gts);
                assert_eq!(upstreams.len(), 1);
                assert!(upstreams[0].starts_with("gts.cf.core.oagw.upstream.v1~"));
                assert!(routes.is_empty());
            }
            other => panic!("expected PluginInUse, got {other:?}"),
        }
    }

    #[test]
    fn plugin_delete_succeeds_when_unused() {
        let svc = flat_svc();
        let plugin_req: PluginRequest = serde_json::from_value(json!({
            "name": "orphan",
            "plugin_type": "transform",
            "config_schema": {},
            "source_code": "def on_request(ctx): pass"
        }))
        .unwrap();
        let plugin = svc.create_plugin(tenant(), &plugin_req).unwrap();
        assert_eq!(
            svc.delete_plugin(tenant(), plugin.id).unwrap(),
            DeleteOutcome::Deleted
        );
    }
}
