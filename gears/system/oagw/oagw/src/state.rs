//! In-memory control plane for the OAGW gear.
//!
//! The spec's reference design stores configuration in a relational store;
//! this crate declares no database dependency, so the control plane is an
//! in-memory, tenant-scoped store holding upstreams, routes, and custom
//! plugins plus the `(tenant_id, alias)` index. All management semantics
//! (tenant scoping, alias derivation/conflicts, route-match uniqueness,
//! plugin-in-use) are implemented here so the API handlers stay thin.

use std::collections::HashMap;
use std::sync::atomic::AtomicU64;
use std::sync::Arc;

use chrono::{SecondsFormat, Utc};
use dashmap::DashMap;
use parking_lot::RwLock;
use pingora_memory_cache::MemoryCache;
use toolkit_canonical_errors::CanonicalError;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::alias;
use crate::api::dto as dto;
use crate::config::OagwConfig;
use crate::error::OagwError;
use crate::gts;
use crate::model::{CustomPlugin, Route, RouteMatch, Upstream};
use crate::proxy::limits::RateLimiter;
use credstore_sdk::CredStoreClientV1;
use tenant_resolver_sdk::TenantResolverClient;
use crate::proxy::oauth::CachedToken;
use crate::tenant::TenantChain;

/// Tenant-scoped configuration tables.
#[derive(Default)]
pub struct ControlPlane {
    upstreams: HashMap<Uuid, Upstream>,
    routes: HashMap<Uuid, Route>,
    plugins: HashMap<Uuid, CustomPlugin>,
    /// `(tenant_id, alias) → upstream_id`.
    alias_index: HashMap<(Uuid, String), Uuid>,
}

/// Shared OAGW gear state handed to API handlers and the data plane.
pub struct OagwState {
    cp: RwLock<ControlPlane>,
    pub config: OagwConfig,
    pub tenant: TenantChain,
    /// Toolkit HTTP client used for all upstream proxying.
    pub http: toolkit_http::HttpClient,
    /// Token-bucket rate limiter (data plane).
    pub rate_limiter: RateLimiter,
    /// OAuth2 client-credentials token cache (ADR-0008).
    pub token_cache: MemoryCache<String, CachedToken>,
    /// Credential store for `cred://` resolution (data plane).
    pub credstore: Option<Arc<dyn CredStoreClientV1>>,
    /// Round-robin counters per upstream (multi-endpoint pools).
    pub rr_counters: DashMap<Uuid, AtomicU64>,
}

impl OagwState {
    /// Build a new state from the resolved gear config.
    pub fn new(config: OagwConfig) -> anyhow::Result<Self> {
        let mut builder = toolkit_http::HttpClientBuilder::new();
        if let Some(timeout) = config.proxy_timeout() {
            builder = builder.timeout(timeout);
        }
        let http = builder
            .build()
            .map_err(|e| anyhow::anyhow!("failed to build OAGW outbound client: {e}"))?;
        Ok(Self {
            cp: RwLock::new(ControlPlane::default()),
            token_cache: MemoryCache::new(config.token_cache_capacity()),
            rate_limiter: RateLimiter::default(),
            credstore: None,
            rr_counters: DashMap::new(),
            config,
            tenant: TenantChain::default(),
            http,
        })
    }

    /// Attach platform services discovered from the client hub (best-effort).
    pub fn attach_dependencies(
        &mut self,
        credstore: Option<Arc<dyn CredStoreClientV1>>,
        tenant_resolver: Option<Arc<dyn TenantResolverClient>>,
    ) {
        self.credstore = credstore;
        if let Some(resolver) = tenant_resolver {
            self.tenant = TenantChain::with_resolver(resolver);
        }
    }

    fn now() -> String {
        Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true)
    }

    // ------------------------------------------------------------------
    // Upstreams
    // ------------------------------------------------------------------

    /// Create an upstream for the caller's tenant.
    pub fn create_upstream(
        &self,
        ctx: &SecurityContext,
        input: dto::UpstreamCreate,
    ) -> Result<Upstream, CanonicalError> {
        let tenant_id = ctx.subject_tenant_id();
        validate_upstream_input(&input)?;

        let derived = derive_alias_from_input(&input);
        let alias = resolve_alias_value(&input, derived)?;
        if !alias::is_valid_alias(&alias) {
            return Err(OagwError::field_violation(
                "alias",
                "alias does not match the required pattern",
                format!("expected ^[a-z0-9]([a-z0-9.:-]*[a-z0-9])?$, got {alias:?}"),
            ));
        }

        let mut cp = self.cp.write();
        if cp.alias_index.contains_key(&(tenant_id, alias.clone())) {
            return Err(OagwError::conflict(
                format!("an upstream with alias {alias:?} already exists for this tenant"),
                alias,
            ));
        }

        let now = Self::now();
        let upstream = Upstream {
            id: Uuid::new_v4(),
            tenant_id,
            enabled: input.enabled,
            alias: alias.clone(),
            tags: input.tags,
            server: input.server,
            protocol: input.protocol,
            auth: input.auth,
            headers: input.headers,
            plugins: input.plugins,
            rate_limit: input.rate_limit,
            cors: input.cors,
            created_at: Some(now.clone()),
            updated_at: Some(now),
        };
        let id = upstream.id;
        cp.alias_index.insert((tenant_id, alias), id);
        cp.upstreams.insert(id, upstream.clone());
        Ok(upstream)
    }

    /// List the calling tenant's upstreams (ancestors are invisible).
    pub fn list_upstreams(&self, tenant_id: Uuid) -> Vec<Upstream> {
        let cp = self.cp.read();
        let mut out: Vec<Upstream> = cp
            .upstreams
            .values()
            .filter(|u| u.tenant_id == tenant_id)
            .cloned()
            .collect();
        out.sort_by(|a, b| a.created_at.cmp(&b.created_at));
        out
    }

    /// Get one upstream owned by the tenant, or `None` (invisible → 404).
    pub fn get_upstream(&self, tenant_id: Uuid, id: Uuid) -> Option<Upstream> {
        let cp = self.cp.read();
        cp.upstreams
            .get(&id)
            .filter(|u| u.tenant_id == tenant_id)
            .cloned()
    }

    /// Replace an entire upstream owned by the tenant.
    pub fn put_upstream(
        &self,
        ctx: &SecurityContext,
        id: Uuid,
        input: dto::UpstreamCreate,
    ) -> Result<Upstream, CanonicalError> {
        let tenant_id = ctx.subject_tenant_id();
        validate_upstream_input(&input)?;

        let derived = derive_alias_from_input(&input);
        let new_alias = resolve_alias_value(&input, derived)?;
        if !alias::is_valid_alias(&new_alias) {
            return Err(OagwError::field_violation(
                "alias",
                "alias does not match the required pattern",
                "invalid alias",
            ));
        }

        let mut cp = self.cp.write();
        let existing = cp
            .upstreams
            .get(&id)
            .filter(|u| u.tenant_id == tenant_id)
            .cloned()
            .ok_or_else(|| OagwError::not_found_resource(id.to_string()))?;

        // Identity conflict: the alias now belongs to another upstream.
        if let Some(owner) = cp.alias_index.get(&(tenant_id, new_alias.clone())).copied() {
            if owner != id {
                return Err(OagwError::conflict(
                    format!("an upstream with alias {new_alias:?} already exists for this tenant"),
                    new_alias,
                ));
            }
        }

        if existing.alias != new_alias {
            cp.alias_index.remove(&(tenant_id, existing.alias.clone()));
            cp.alias_index.insert((tenant_id, new_alias.clone()), id);
        }

        let now = Self::now();
        let upstream = Upstream {
            id,
            tenant_id,
            enabled: input.enabled,
            alias: new_alias,
            tags: input.tags,
            server: input.server,
            protocol: input.protocol,
            auth: input.auth,
            headers: input.headers,
            plugins: input.plugins,
            rate_limit: input.rate_limit,
            cors: input.cors,
            created_at: existing.created_at,
            updated_at: Some(now),
        };
        cp.upstreams.insert(id, upstream.clone());
        Ok(upstream)
    }

    /// Delete an upstream owned by the tenant, cascading to its routes.
    pub fn delete_upstream(&self, ctx: &SecurityContext, id: Uuid) -> Result<(), CanonicalError> {
        let tenant_id = ctx.subject_tenant_id();
        let mut cp = self.cp.write();
        let existing = cp
            .upstreams
            .get(&id)
            .filter(|u| u.tenant_id == tenant_id)
            .cloned()
            .ok_or_else(|| OagwError::not_found_resource(id.to_string()))?;
        cp.alias_index.remove(&(tenant_id, existing.alias));
        cp.upstreams.remove(&id);
        cp.routes.retain(|_, r| r.upstream_id != id);
        Ok(())
    }

    // ------------------------------------------------------------------
    // Routes
    // ------------------------------------------------------------------

    /// Create a route against one of the tenant's own upstreams.
    pub fn create_route(
        &self,
        ctx: &SecurityContext,
        input: dto::RouteCreate,
    ) -> Result<Route, CanonicalError> {
        let tenant_id = ctx.subject_tenant_id();
        validate_route_match(&input.r#match)?;

        let mut cp = self.cp.write();
        cp.upstreams
            .get(&input.upstream_id)
            .filter(|u| u.tenant_id == tenant_id)
            .cloned()
            .ok_or_else(|| OagwError::not_found_resource(input.upstream_id.to_string()))?;

        if let Some(conflict) = conflicting_route(&cp, &input.upstream_id, &input.r#match) {
            return Err(OagwError::conflict(
                "a route with the same path and method already exists for this upstream",
                conflict,
            ));
        }

        let now = Self::now();
        let route = Route {
            id: Uuid::new_v4(),
            tenant_id,
            tags: input.tags,
            upstream_id: input.upstream_id,
            r#match: input.r#match,
            plugins: input.plugins,
            rate_limit: input.rate_limit,
            created_at: Some(now.clone()),
            updated_at: Some(now),
        };
        let id = route.id;
        cp.routes.insert(id, route.clone());
        Ok(route)
    }

    /// List the calling tenant's routes.
    pub fn list_routes(&self, tenant_id: Uuid) -> Vec<Route> {
        let cp = self.cp.read();
        let mut out: Vec<Route> = cp
            .routes
            .values()
            .filter(|r| r.tenant_id == tenant_id)
            .cloned()
            .collect();
        out.sort_by(|a, b| a.created_at.cmp(&b.created_at));
        out
    }

    /// Get one route owned by the tenant.
    pub fn get_route(&self, tenant_id: Uuid, id: Uuid) -> Option<Route> {
        let cp = self.cp.read();
        cp.routes
            .get(&id)
            .filter(|r| r.tenant_id == tenant_id)
            .cloned()
    }

    /// Replace a route (upstream_id is immutable; absent from the DTO).
    pub fn put_route(
        &self,
        ctx: &SecurityContext,
        id: Uuid,
        input: dto::RoutePut,
    ) -> Result<Route, CanonicalError> {
        let tenant_id = ctx.subject_tenant_id();
        validate_route_match(&input.r#match)?;

        let mut cp = self.cp.write();
        let existing = cp
            .routes
            .get(&id)
            .filter(|r| r.tenant_id == tenant_id)
            .cloned()
            .ok_or_else(|| OagwError::not_found_resource(id.to_string()))?;

        // If the shape changed into a collision, reject; an unchanged shape
        // re-put is the idempotent update path and must pass.
        if existing.r#match != input.r#match {
            if let Some(conflict) = conflicting_route(&cp, &existing.upstream_id, &input.r#match) {
                return Err(OagwError::conflict(
                    "a route with the same path and method already exists for this upstream",
                    conflict,
                ));
            }
        }

        let now = Self::now();
        let route = Route {
            id,
            tenant_id,
            tags: input.tags,
            upstream_id: existing.upstream_id,
            r#match: input.r#match,
            plugins: input.plugins,
            rate_limit: input.rate_limit,
            created_at: existing.created_at,
            updated_at: Some(now),
        };
        cp.routes.insert(id, route.clone());
        Ok(route)
    }

    /// Delete a route owned by the tenant.
    pub fn delete_route(&self, ctx: &SecurityContext, id: Uuid) -> Result<(), CanonicalError> {
        let tenant_id = ctx.subject_tenant_id();
        let mut cp = self.cp.write();
        let present = cp
            .routes
            .get(&id)
            .is_some_and(|r| r.tenant_id == tenant_id);
        if !present {
            return Err(OagwError::not_found_resource(id.to_string()));
        }
        cp.routes.remove(&id);
        Ok(())
    }

    // ------------------------------------------------------------------
    // Plugins
    // ------------------------------------------------------------------

    /// Create a custom plugin (immutable after creation).
    pub fn create_plugin(
        &self,
        ctx: &SecurityContext,
        input: dto::PluginCreate,
    ) -> Result<CustomPlugin, CanonicalError> {
        let tenant_id = ctx.subject_tenant_id();
        if input.name.trim().is_empty() {
            return Err(OagwError::field_violation(
                "name",
                "plugin name must not be empty",
                "required",
            ));
        }
        if gts::plugin_base(&input.plugin_type).is_empty() {
            return Err(OagwError::field_violation(
                "plugin_type",
                format!("{:?} is not a recognized OAGW plugin type", input.plugin_type),
                "unknown plugin type",
            ));
        }

        let mut cp = self.cp.write();
        if cp
            .plugins
            .values()
            .any(|p| p.tenant_id == tenant_id && p.name == input.name)
        {
            return Err(OagwError::conflict(
                format!("a plugin named {:?} already exists for this tenant", input.name),
                input.name,
            ));
        }

        let plugin = CustomPlugin {
            id: Uuid::new_v4(),
            tenant_id,
            name: input.name,
            plugin_type: input.plugin_type,
            config: input.config,
            source: input.source,
            created_at: Some(Self::now()),
        };
        let id = plugin.id;
        cp.plugins.insert(id, plugin.clone());
        Ok(plugin)
    }

    /// List the calling tenant's custom plugins.
    pub fn list_plugins(&self, tenant_id: Uuid) -> Vec<CustomPlugin> {
        let cp = self.cp.read();
        let mut out: Vec<CustomPlugin> = cp
            .plugins
            .values()
            .filter(|p| p.tenant_id == tenant_id)
            .cloned()
            .collect();
        out.sort_by(|a, b| a.created_at.cmp(&b.created_at));
        out
    }

    /// Get one custom plugin owned by the tenant.
    pub fn get_plugin(&self, tenant_id: Uuid, id: Uuid) -> Option<CustomPlugin> {
        let cp = self.cp.read();
        cp.plugins
            .get(&id)
            .filter(|p| p.tenant_id == tenant_id)
            .cloned()
    }

    /// Delete a custom plugin, refusing when still referenced by an
    /// upstream or route binding (409 PluginInUse).
    pub fn delete_plugin(&self, ctx: &SecurityContext, id: Uuid) -> Result<(), CanonicalError> {
        let tenant_id = ctx.subject_tenant_id();
        let mut cp = self.cp.write();
        let present = cp
            .plugins
            .get(&id)
            .is_some_and(|p| p.tenant_id == tenant_id);
        if !present {
            return Err(OagwError::not_found_resource(id.to_string()));
        }
        let uuid_str = id.to_string();
        let referenced = cp
            .upstreams
            .values()
            .any(|u| references_plugin_uuid(u, &uuid_str))
            || cp
                .routes
                .values()
                .any(|r| references_plugin_uuid(r, &uuid_str));
        if referenced {
            return Err(OagwError::conflict(
                "plugin is still referenced by an upstream or route",
                uuid_str,
            ));
        }
        cp.plugins.remove(&id);
        Ok(())
    }

    // ------------------------------------------------------------------
    // Data-plane accessors
    // ------------------------------------------------------------------

    /// Resolve the closest enabled upstream by alias across the tenant
    /// chain (nearest tenant wins; a disabled nearest binding shadows
    /// ancestors entirely).
    pub fn resolve_upstream(&self, chain: &[Uuid], alias: &str) -> Option<Upstream> {
        let cp = self.cp.read();
        for tenant in chain {
            if let Some(id) = cp.alias_index.get(&(*tenant, alias.to_string())) {
                if let Some(u) = cp.upstreams.get(id).filter(|u| u.enabled) {
                    return Some(u.clone());
                }
                return None;
            }
        }
        None
    }

    /// All `(tenant_id, upstream)` records along the chain carrying `alias`,
    /// nearest first — the data plane uses this for config merge.
    pub fn upstream_bindings(&self, chain: &[Uuid], alias: &str) -> Vec<(Uuid, Upstream)> {
        let cp = self.cp.read();
        chain
            .iter()
            .filter_map(|tenant| {
                cp.alias_index
                    .get(&(*tenant, alias.to_string()))
                    .and_then(|id| cp.upstreams.get(id).cloned())
                    .map(|u| (*tenant, u))
            })
            .collect()
    }

    /// Routes belonging to any tenant in the chain and targeted at
    /// `upstream_id`, nearest first.
    pub fn routes_for_upstream(&self, chain: &[Uuid], upstream_id: Uuid) -> Vec<(Uuid, Route)> {
        let cp = self.cp.read();
        let mut out: Vec<(Uuid, Route)> = Vec::new();
        for tenant in chain {
            let mut own: Vec<(Uuid, Route)> = cp
                .routes
                .values()
                .filter(|r| r.tenant_id == *tenant && r.upstream_id == upstream_id)
                .map(|r| (*tenant, r.clone()))
                .collect();
            out.append(&mut own);
        }
        out
    }

    /// Look up a custom plugin by UUID in the calling tenant's store.
    pub fn plugin_by_uuid(&self, tenant_id: Uuid, id: Uuid) -> Option<CustomPlugin> {
        let cp = self.cp.read();
        cp.plugins
            .get(&id)
            .filter(|p| p.tenant_id == tenant_id)
            .cloned()
    }
}

// ---------------------------------------------------------------------------
// Validation helpers
// ---------------------------------------------------------------------------

fn derive_alias_from_input(input: &dto::UpstreamCreate) -> Option<String> {
    let endpoints: Vec<(String, String, Option<u16>)> = input
        .server
        .endpoints
        .iter()
        .map(|e| (e.scheme.clone(), e.host.clone(), e.port))
        .collect();
    alias::derive_alias(&endpoints)
}

fn resolve_alias_value(
    input: &dto::UpstreamCreate,
    derived: Option<String>,
) -> Result<String, CanonicalError> {
    match (&input.alias, derived) {
        (Some(explicit), Some(derived)) => {
            if explicit == &derived {
                Ok(explicit.clone())
            } else {
                Err(OagwError::field_violation(
                    "alias",
                    "explicit alias conflicts with the derived alias",
                    format!("hostname endpoints derive alias {derived:?}"),
                ))
            }
        }
        (None, Some(derived)) => Ok(derived),
        (Some(explicit), None) => Ok(explicit.clone()),
        (None, None) => Err(OagwError::field_violation(
            "alias",
            "explicit alias is required for IP-based or non-derivable endpoints",
            "required",
        )),
    }
}

/// Field-level validation mirroring `upstream.v1.schema.json`.
fn validate_upstream_input(input: &dto::UpstreamCreate) -> Result<(), CanonicalError> {
    let mut violations: Vec<(String, String, String)> = Vec::new();

    if input.server.endpoints.is_empty() {
        violations.push((
            "server.endpoints".into(),
            "at least one endpoint is required".into(),
            "minItems: 1".into(),
        ));
    }
    for (i, e) in input.server.endpoints.iter().enumerate() {
        let path = format!("server.endpoints[{i}]");
        if !alias::scheme_is_proxyable(&e.scheme) {
            violations.push((
                format!("{path}.scheme"),
                format!("scheme {:?} is not allowed", e.scheme),
                "allowed: https, wss, wt, grpc".into(),
            ));
        }
        if e.host.trim().is_empty() {
            violations.push((
                format!("{path}.host"),
                "host must not be empty".into(),
                "minLength: 1".into(),
            ));
        }
        if e.port == Some(0) {
            violations.push((
                format!("{path}.port"),
                "port out of range 1..=65535".into(),
                "range".into(),
            ));
        }
    }

    if input.protocol != gts::PROTOCOL_HTTP && input.protocol != gts::PROTOCOL_GRPC {
        violations.push((
            "protocol".into(),
            format!("unknown protocol {:?}", input.protocol),
            "expected an OAGW protocol GTS identifier".into(),
        ));
    }

    if let Some(auth) = &input.auth {
        validate_auth_config(&mut violations, auth);
    }

    validate_plugin_bindings(&input.plugins.items, &mut violations, "plugins.items");
    validate_cors(&mut violations, input.cors.as_ref());
    validate_rate_limit(&mut violations, input.rate_limit.as_ref());

    if violations.is_empty() {
        Ok(())
    } else {
        Err(OagwError::violations(violations))
    }
}

fn validate_auth_config(
    violations: &mut Vec<(String, String, String)>,
    auth: &crate::model::AuthConfig,
) {
    // `basic` / `bearer` are catalog-only (DESIGN §3.5.1): binding them to an
    // upstream fails with "unknown auth plugin" — the data plane has no
    // implementation for them.
    let known_builtin = matches!(
        auth.auth_type.as_str(),
        gts::AUTH_NOOP | gts::AUTH_APIKEY | gts::AUTH_OAUTH2_CC | gts::AUTH_OAUTH2_CC_BASIC
    );
    let known_custom = gts::is_uuid_instance(&auth.auth_type)
        && auth.auth_type.starts_with(gts::AUTH_PLUGIN_TYPE);
    if !known_builtin && !known_custom {
        violations.push((
            "auth.type".into(),
            format!("unknown auth plugin {:?}", auth.auth_type),
            "not a resolvable auth plugin identifier".into(),
        ));
    }
    // OAuth2 client-credentials plugins require an endpoint + client creds.
    if auth.auth_type == gts::AUTH_OAUTH2_CC || auth.auth_type == gts::AUTH_OAUTH2_CC_BASIC {
        let cfg = &auth.config;
        let endpoint_ok = cfg.get("token_endpoint").is_some() || cfg.get("issuer_url").is_some();
        let creds_ok = cfg.get("client_id_ref").is_some() && cfg.get("client_secret_ref").is_some();
        if !endpoint_ok {
            violations.push((
                "auth.config".into(),
                "oauth2 client-credentials requires token_endpoint or issuer_url".into(),
                "mutually exclusive endpoint configuration".into(),
            ));
        }
        if !creds_ok {
            violations.push((
                "auth.config".into(),
                "oauth2 client-credentials requires client_id_ref and client_secret_ref".into(),
                "cred:// references required".into(),
            ));
        }
    }
}

/// Verify every plugin binding resolves to a known builtin guard/transform,
/// or is a custom-plugin UUID reference (resolved at bind time).
fn validate_plugin_bindings(
    items: &[String],
    violations: &mut Vec<(String, String, String)>,
    path: &str,
) {
    const KNOWN_GUARDS: [&str; 1] = [gts::GUARD_REQUIRED_HEADERS];
    const KNOWN_TRANSFORMS: [&str; 1] = [gts::TRANSFORM_REQUEST_ID];
    for (i, item) in items.iter().enumerate() {
        let builtin_ok = match gts::plugin_base(item) {
            gts::GUARD_PLUGIN_TYPE => KNOWN_GUARDS.contains(&item.as_str()),
            gts::TRANSFORM_PLUGIN_TYPE => KNOWN_TRANSFORMS.contains(&item.as_str()),
            _ => false,
        };
        let custom_ok = gts::is_uuid_instance(item) && !gts::plugin_base(item).is_empty();
        if !builtin_ok && !custom_ok {
            violations.push((
                format!("{path}[{i}]"),
                format!("plugin binding {item:?} does not resolve"),
                "expected a builtin guard/transform GTS identifier or custom plugin UUID"
                    .into(),
            ));
        }
    }
}

fn validate_cors(
    violations: &mut Vec<(String, String, String)>,
    cors: Option<&crate::model::CorsConfig>,
) {
    if let Some(cors) = cors {
        if cors.allow_credentials && cors.allowed_origins.iter().any(|o| o == "*") {
            violations.push((
                "cors".into(),
                "allow_credentials cannot be combined with a wildcard origin".into(),
                "specific origins required when allow_credentials is true".into(),
            ));
        }
    }
}

fn validate_rate_limit(
    violations: &mut Vec<(String, String, String)>,
    rl: Option<&crate::model::RateLimitConfig>,
) {
    if let Some(rl) = rl {
        if rl.sustained.rate < 1 {
            violations.push((
                "rate_limit.sustained.rate".into(),
                "sustained rate must be at least 1".into(),
                "minimum: 1".into(),
            ));
        }
        if let Some(b) = &rl.burst {
            if b.capacity < 1 {
                violations.push((
                    "rate_limit.burst.capacity".into(),
                    "burst capacity must be at least 1".into(),
                    "minimum: 1".into(),
                ));
            }
        }
    }
}

fn validate_route_match(m: &RouteMatch) -> Result<(), CanonicalError> {
    let mut violations: Vec<(String, String, String)> = Vec::new();
    match (&m.http, &m.grpc) {
        (Some(_), Some(_)) | (None, None) => violations.push((
            "match".into(),
            "exactly one of http or grpc must be present".into(),
            "oneOf".into(),
        )),
        (Some(h), None) => {
            if h.methods.is_empty() {
                violations.push((
                    "match.http.methods".into(),
                    "at least one method is required".into(),
                    "minItems: 1".into(),
                ));
            }
            for (i, m) in h.methods.iter().enumerate() {
                if !matches!(m.as_str(), "GET" | "POST" | "PUT" | "DELETE" | "PATCH") {
                    violations.push((
                        format!("match.http.methods[{i}]"),
                        format!("unsupported method {m:?}"),
                        "allowed: GET, POST, PUT, DELETE, PATCH".into(),
                    ));
                }
            }
            if h.path.trim().is_empty() {
                violations.push((
                    "match.http.path".into(),
                    "path must not be empty".into(),
                    "minLength: 1".into(),
                ));
            }
        }
        (None, Some(g)) => {
            if g.service.trim().is_empty() {
                violations.push((
                    "match.grpc.service".into(),
                    "service must not be empty".into(),
                    "minLength: 1".into(),
                ));
            }
            if g.method.trim().is_empty() {
                violations.push((
                    "match.grpc.method".into(),
                    "method must not be empty".into(),
                    "minLength: 1".into(),
                ));
            }
        }
    }
    if violations.is_empty() {
        Ok(())
    } else {
        Err(OagwError::violations(violations))
    }
}

fn conflicting_route(cp: &ControlPlane, upstream_id: &Uuid, m: &RouteMatch) -> Option<String> {
    let (path, methods) = match &m.http {
        Some(h) => (h.path.clone(), h.methods.clone()),
        None => return None,
    };
    for r in cp.routes.values() {
        if &r.upstream_id != upstream_id {
            continue;
        }
        if let Some(h) = &r.r#match.http {
            if h.path == path && h.methods.iter().any(|x| methods.contains(x)) {
                return Some(r.id.to_string());
            }
        }
    }
    None
}

/// Whether an upstream/route references a custom plugin by UUID anywhere.
fn references_plugin_uuid(resource: &impl PluginRefHolder, uuid: &str) -> bool {
    resource.plugin_refs().iter().any(|p| {
        p == uuid || p.ends_with(&format!("~{uuid}")) || p.split('~').any(|part| part == uuid)
    })
}

/// Trait to gather plugin references from an upstream or route.
pub trait PluginRefHolder {
    fn plugin_refs(&self) -> Vec<String>;
}

impl PluginRefHolder for Upstream {
    fn plugin_refs(&self) -> Vec<String> {
        let mut refs = self.plugins.items.clone();
        if let Some(auth) = &self.auth {
            if gts::is_uuid_instance(&auth.auth_type) {
                refs.push(auth.auth_type.clone());
            }
        }
        refs
    }
}

impl PluginRefHolder for Route {
    fn plugin_refs(&self) -> Vec<String> {
        self.plugins.items.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use toolkit_security::SecurityContext;

    fn ctx() -> SecurityContext {
        SecurityContext::builder()
            .subject_id(Uuid::new_v4())
            .subject_tenant_id(Uuid::new_v4())
            .build()
            .unwrap()
    }

    fn base_input() -> dto::UpstreamCreate {
        dto::UpstreamCreate {
            enabled: true,
            alias: None,
            tags: vec![],
            server: crate::model::UpstreamServer {
                endpoints: vec![crate::model::Endpoint {
                    host: "api.example.com".into(),
                    port: Some(443),
                    scheme: "https".into(),
                }],
            },
            protocol: gts::PROTOCOL_HTTP.to_string(),
            auth: None,
            headers: None,
            plugins: crate::model::PluginsConfig::default(),
            rate_limit: None,
            cors: None,
        }
    }

    #[tokio::test]
    async fn create_upstream_derives_alias() {
        let state = OagwState::new(OagwConfig::default()).unwrap();
        let c = ctx();
        let u = state.create_upstream(&c, base_input()).unwrap();
        assert_eq!(u.alias, "api.example.com");
        assert_eq!(u.created_at, u.updated_at);
    }

    #[tokio::test]
    async fn duplicate_alias_conflicts() {
        let state = OagwState::new(OagwConfig::default()).unwrap();
        let c = ctx();
        state.create_upstream(&c, base_input()).unwrap();
        let err = state.create_upstream(&c, base_input()).unwrap_err();
        assert_eq!(err.status_code(), 409);
    }

    #[tokio::test]
    async fn ip_endpoint_requires_explicit_alias() {
        let state = OagwState::new(OagwConfig::default()).unwrap();
        let c = ctx();
        let mut input = base_input();
        input.server.endpoints[0].host = "10.0.0.1".into();
        let err = state.create_upstream(&c, input.clone()).unwrap_err();
        assert_eq!(err.status_code(), 400);
        input.alias = Some("my-explicit-alias".into());
        let u = state.create_upstream(&c, input).unwrap();
        assert_eq!(u.alias, "my-explicit-alias");
    }

    #[tokio::test]
    async fn unknown_auth_type_is_validation_error() {
        let state = OagwState::new(OagwConfig::default()).unwrap();
        let c = ctx();
        let mut input = base_input();
        input.auth = Some(crate::model::AuthConfig {
            auth_type: "gts.cf.core.oagw.auth_plugin.v1~completely.unknown.v1".into(),
            sharing: crate::model::SharingMode::Private,
            config: json!({}),
        });
        let err = state.create_upstream(&c, input).unwrap_err();
        assert_eq!(err.status_code(), 400);
    }

    #[tokio::test]
    async fn route_requires_own_upstream() {
        let state = OagwState::new(OagwConfig::default()).unwrap();
        let c = ctx();
        let input = dto::RouteCreate {
            tags: vec![],
            upstream_id: Uuid::new_v4(),
            r#match: RouteMatch {
                http: Some(crate::model::HttpMatch {
                    methods: vec!["GET".into()],
                    path: "/v1/things".into(),
                    query_allowlist: vec![],
                    path_suffix_mode: crate::model::PathSuffixMode::Append,
                }),
                grpc: None,
            },
            plugins: crate::model::PluginsConfig::default(),
            rate_limit: None,
        };
        let err = state.create_route(&c, input).unwrap_err();
        assert_eq!(err.status_code(), 404);
    }

    #[tokio::test]
    async fn plugin_in_use_blocks_delete() {
        let state = OagwState::new(OagwConfig::default()).unwrap();
        let c = ctx();
        let p = state
            .create_plugin(
                &c,
                dto::PluginCreate {
                    name: "req-id".into(),
                    plugin_type: format!("{}~cf.core.oagw.req_id.v1", gts::TRANSFORM_PLUGIN_TYPE),
                    config: json!({}),
                    source: None,
                },
            )
            .unwrap();
        let mut input = base_input();
        input.plugins.items.push(format!("{}~{}", gts::TRANSFORM_PLUGIN_TYPE, p.id));
        state.create_upstream(&c, input).unwrap();
        let err = state.delete_plugin(&c, p.id).unwrap_err();
        assert_eq!(err.status_code(), 409);
    }
}
