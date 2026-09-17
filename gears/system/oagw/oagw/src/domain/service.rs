//! OAGW domain service: control-plane CRUD and the data-plane proxy
//! pipeline (DESIGN §3.2, §3.3).
//!
//! Storage is in-memory (`DashMap`), tenant-scoped by `tenant_id`
//! (single-tenant static deployments today). The proxy pipeline follows the
//! DESIGN sequence:
//!
//! 1. Resolve alias → upstream (enabled only)
//! 2. Endpoint selection (`X-OAGW-Target-Host` matrix)
//! 3. Route match (longest path prefix + method + query allowlist +
//!    path-suffix mode)
//! 4. Effective config merge (rate limit, CORS)
//! 5. CORS actual-request validation
//! 6. Rate limiting (token bucket, reject/queue/degrade)
//! 7. Auth plugin → guards → request transforms
//! 8. Forward upstream (timeout + body cap)
//! 9. Response guards/transforms + `X-OAGW-Error-Source`

use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode};
use bytes::Bytes;
use dashmap::DashMap;
use parking_lot::Mutex;
use toolkit_http::HttpClient;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::config::OagwConfig;
use crate::domain::alias::{resolve_create_alias, resolve_update_alias};
use crate::domain::ids::{UPSTREAM_TYPE, extract_resource_uuid};
use crate::domain::model::{
    CorsConfig, Endpoint, PluginBinding, PluginChainConfig, PluginKind, RateLimitConfig,
    RouteConfig, StoredPlugin, StoredRoute, StoredUpstream, UpstreamConfig,
};
use crate::domain::plugin::{AuthContext, GuardContext, PluginError, TransformContext};
use crate::error::OagwError;
use crate::infra::outbound::{
    OutboundRequest, build_url, forward, forward_error_to_oagw, sanitize_inbound_headers,
    sanitize_outbound_headers, upstream_rate_limit_headers,
};
use crate::infra::plugins::PluginRegistry;
use crate::infra::ratelimit::{RateLimitDecision, RateLimiter, effective_rate_limit};

/// Result of a successful proxy request.
pub struct ProxyOutcome {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub body: Bytes,
}

/// Round-robin cursor for multi-endpoint pools without a target header.
static ROUND_ROBIN: AtomicU64 = AtomicU64::new(0);

/// The OAGW domain service.
pub struct OagwService {
    config: OagwConfig,
    client: HttpClient,
    /// Toolkit client hub (attached at gear init) for lazy `credstore`
    /// resolution.
    client_hub: Mutex<Option<Arc<toolkit::ClientHub>>>,
    upstreams: DashMap<Uuid, StoredUpstream>,
    routes: DashMap<Uuid, StoredRoute>,
    plugins: DashMap<Uuid, StoredPlugin>,
    registry: PluginRegistry,
    limiter: RateLimiter,
}

impl OagwService {
    /// Create the service from gear config + an outbound HTTP client.
    #[must_use]
    pub fn new(config: OagwConfig, client: HttpClient) -> Self {
        let token_cache = crate::infra::plugins::TokenCacheConfig {
            ttl: Duration::from_secs(u64::from(config.token_cache_ttl_secs)),
            capacity: config.token_cache_capacity,
        };
        Self {
            registry: PluginRegistry::with_builtins(&token_cache),
            config,
            client,
            client_hub: Mutex::new(None),
            upstreams: DashMap::new(),
            routes: DashMap::new(),
            plugins: DashMap::new(),
            limiter: RateLimiter::new(),
        }
    }

    /// Attach the toolkit client hub (called by the gear at init) so the
    /// proxy pipeline can resolve the `credstore` client lazily.
    pub fn attach_client_hub(&self, hub: Arc<toolkit::ClientHub>) {
        *self.client_hub.lock() = Some(hub);
    }

    /// Number of stored upstreams (test/observability hook).
    #[must_use]
    pub fn upstream_count(&self) -> usize {
        self.upstreams.len()
    }

    #[must_use]
    pub fn route_count(&self) -> usize {
        self.routes.len()
    }

    #[must_use]
    pub fn plugin_count(&self) -> usize {
        self.plugins.len()
    }

    fn max_body_bytes(&self) -> usize {
        self.config.max_body_bytes
    }

    fn proxy_timeout(&self) -> Duration {
        self.config.proxy_timeout()
    }

    // -------------------------------------------------------------------
    // Control Plane: upstreams
    // -------------------------------------------------------------------

    /// Create an upstream. Alias rules are enforced via
    /// [`resolve_create_alias`]; `(tenant_id, alias)` is unique (409).
    pub fn create_upstream(
        &self,
        tenant_id: Uuid,
        config: UpstreamConfig,
    ) -> Result<StoredUpstream, OagwError> {
        validate_upstream_config(&config)?;
        if let Some(cors) = &config.cors {
            if let Some(msg) = cors.validation_error() {
                return Err(OagwError::Validation(msg));
            }
        }
        let resolution = resolve_create_alias(config.alias.as_deref(), &config.server.endpoints)
            .map_err(OagwError::Validation)?;

        // Per-tenant alias uniqueness.
        let alias = resolution.alias.clone();
        let taken = self
            .upstreams
            .iter()
            .any(|u| u.tenant_id == tenant_id && u.alias == alias);
        if taken {
            return Err(OagwError::Validation(format!(
                "alias '{alias}' is already in use by this tenant"
            )));
        }

        let now = now_secs();
        let stored = StoredUpstream {
            id: Uuid::new_v4(),
            tenant_id,
            alias,
            alias_derived: resolution.derived,
            config,
            created_at: now,
            updated_at: now,
        };
        self.upstreams.insert(stored.id, stored.clone());
        Ok(stored)
    }

    /// Get an upstream by id (UUID or anonymous GTS id), tenant-scoped.
    pub fn get_upstream(&self, tenant_id: Uuid, raw_id: &str) -> Result<StoredUpstream, OagwError> {
        let id = extract_resource_uuid(raw_id, UPSTREAM_TYPE)
            .ok_or_else(|| OagwError::NotFound("upstream".to_owned()))?;
        self.upstreams
            .get(&id)
            .filter(|u| u.tenant_id == tenant_id)
            .map(|u| u.clone())
            .ok_or_else(|| OagwError::NotFound("upstream".to_owned()))
    }

    /// Resolve an upstream by its routing alias (tenant-scoped).
    #[must_use]
    pub fn get_upstream_by_alias(&self, tenant_id: Uuid, alias: &str) -> Option<StoredUpstream> {
        let alias = crate::domain::alias::normalize(alias);
        self.upstreams
            .iter()
            .filter(|u| u.tenant_id == tenant_id)
            .find(|u| u.alias == alias)
            .map(|u| u.clone())
    }

    /// List tenant upstreams (optionally filtered by alias).
    pub fn list_upstreams(&self, tenant_id: Uuid) -> Vec<StoredUpstream> {
        let mut items: Vec<StoredUpstream> = self
            .upstreams
            .iter()
            .filter(|u| u.tenant_id == tenant_id)
            .map(|u| u.clone())
            .collect();
        items.sort_by_key(|u| u.created_at);
        items
    }

    /// Replace an upstream (PUT semantics). Alias immutability is enforced
    /// via [`resolve_update_alias`]; `upstream_id` is immutable.
    pub fn update_upstream(
        &self,
        tenant_id: Uuid,
        raw_id: &str,
        config: UpstreamConfig,
    ) -> Result<StoredUpstream, OagwError> {
        let id = extract_resource_uuid(raw_id, UPSTREAM_TYPE)
            .ok_or_else(|| OagwError::NotFound("upstream".to_owned()))?;
        // Read the existing entry up-front and release the DashMap guard
        // before any `.iter()`-based conflict checks below: DashMap's
        // `iter()` locks every shard, so holding a `get_mut` guard across it
        // would self-deadlock.
        let old = self
            .upstreams
            .get(&id)
            .filter(|u| u.tenant_id == tenant_id)
            .map(|u| (*u).clone())
            .ok_or_else(|| OagwError::NotFound("upstream".to_owned()))?;

        validate_upstream_config(&config)?;
        if let Some(cors) = &config.cors {
            if let Some(msg) = cors.validation_error() {
                return Err(OagwError::Validation(msg));
            }
        }
        let alias = resolve_update_alias(
            &old.alias,
            old.alias_derived,
            config.alias.as_deref(),
            &config.server.endpoints,
        )
        .map_err(OagwError::Validation)?;

        // Alias conflict with a *different* upstream in the same tenant.
        let conflict = self
            .upstreams
            .iter()
            .any(|u| u.id != id && u.tenant_id == tenant_id && u.alias == alias);
        if conflict {
            return Err(OagwError::Validation(format!(
                "alias '{alias}' is already in use by this tenant"
            )));
        }

        let stored = StoredUpstream {
            id: old.id,
            tenant_id: old.tenant_id,
            alias,
            alias_derived: old.alias_derived,
            config,
            created_at: old.created_at,
            updated_at: now_secs(),
        };
        self.upstreams.insert(stored.id, stored.clone());
        Ok(stored)
    }

    /// Delete an upstream and cascade-delete its routes.
    pub fn delete_upstream(&self, tenant_id: Uuid, raw_id: &str) -> Result<(), OagwError> {
        let id = extract_resource_uuid(raw_id, UPSTREAM_TYPE)
            .ok_or_else(|| OagwError::NotFound("upstream".to_owned()))?;
        let existing = self
            .upstreams
            .get(&id)
            .filter(|u| u.tenant_id == tenant_id)
            .map(|u| (*u).clone())
            .ok_or_else(|| OagwError::NotFound("upstream".to_owned()))?;
        self.upstreams.remove(&id);
        self.routes
            .retain(|_, r| !(r.tenant_id == tenant_id && r.upstream_id == id));
        let _ = existing;
        // Prune rate-limit buckets no longer referenced.
        let live: HashSet<String> = self
            .upstreams
            .iter()
            .filter(|u| u.tenant_id == tenant_id)
            .map(|u| u.alias.clone())
            .collect();
        self.limiter.retain(&live);
        Ok(())
    }

    // -------------------------------------------------------------------
    // Control Plane: routes
    // -------------------------------------------------------------------

    /// Create a route for a tenant-owned upstream. `upstream_id` may be a
    /// bare UUID or anonymous GTS id; match-rule uniqueness is enforced
    /// (same method + path under the same upstream → 400).
    pub fn create_route(
        &self,
        tenant_id: Uuid,
        config: RouteConfig,
    ) -> Result<StoredRoute, OagwError> {
        let upstream_id = self.resolve_own_upstream(tenant_id, &config.upstream_id)?;
        validate_route_config(&config)?;
        self.check_route_conflict(tenant_id, upstream_id, None, &config)?;

        let now = now_secs();
        let stored = StoredRoute {
            id: Uuid::new_v4(),
            tenant_id,
            upstream_id,
            config,
            created_at: now,
            updated_at: now,
        };
        self.routes.insert(stored.id, stored.clone());
        Ok(stored)
    }

    pub fn get_route(&self, tenant_id: Uuid, raw_id: &str) -> Result<StoredRoute, OagwError> {
        let id = crate::domain::ids::extract_resource_uuid(raw_id, crate::domain::ids::ROUTE_TYPE)
            .ok_or_else(|| OagwError::NotFound("route".to_owned()))?;
        self.routes
            .get(&id)
            .filter(|r| r.tenant_id == tenant_id)
            .map(|r| r.clone())
            .ok_or_else(|| OagwError::NotFound("route".to_owned()))
    }

    pub fn list_routes(&self, tenant_id: Uuid, upstream_id: Option<Uuid>) -> Vec<StoredRoute> {
        let mut items: Vec<StoredRoute> = self
            .routes
            .iter()
            .filter(|r| {
                r.tenant_id == tenant_id && upstream_id.map_or(true, |uid| r.upstream_id == uid)
            })
            .map(|r| r.clone())
            .collect();
        items.sort_by_key(|r| r.created_at);
        items
    }

    /// Replace a route (PUT). `upstream_id` is immutable — it is not taken
    /// from the replacement body.
    pub fn update_route(
        &self,
        tenant_id: Uuid,
        raw_id: &str,
        config: RouteConfig,
    ) -> Result<StoredRoute, OagwError> {
        let id = crate::domain::ids::extract_resource_uuid(raw_id, crate::domain::ids::ROUTE_TYPE)
            .ok_or_else(|| OagwError::NotFound("route".to_owned()))?;
        let mut existing = self
            .routes
            .get_mut(&id)
            .filter(|r| r.tenant_id == tenant_id)
            .ok_or_else(|| OagwError::NotFound("route".to_owned()))?;
        let old = existing.clone();

        validate_route_config(&config)?;
        self.check_route_conflict(tenant_id, old.upstream_id, Some(id), &config)?;

        let stored = StoredRoute {
            id: old.id,
            tenant_id: old.tenant_id,
            upstream_id: old.upstream_id,
            config,
            created_at: old.created_at,
            updated_at: now_secs(),
        };
        *existing = stored.clone();
        Ok(stored)
    }

    pub fn delete_route(&self, tenant_id: Uuid, raw_id: &str) -> Result<(), OagwError> {
        let id = crate::domain::ids::extract_resource_uuid(raw_id, crate::domain::ids::ROUTE_TYPE)
            .ok_or_else(|| OagwError::NotFound("route".to_owned()))?;
        let found = self
            .routes
            .get(&id)
            .filter(|r| r.tenant_id == tenant_id)
            .is_some();
        if !found {
            return Err(OagwError::NotFound("route".to_owned()));
        }
        self.routes.remove(&id);
        Ok(())
    }

    // -------------------------------------------------------------------
    // Control Plane: plugins
    // -------------------------------------------------------------------

    /// Create a custom plugin from its GTS kind identifier. The kind is
    /// parsed from the identifier; the stored id is a fresh UUID.
    pub fn create_plugin(
        &self,
        tenant_id: Uuid,
        name: String,
        requested_gts_id: String,
        source_code: String,
        config_schema: serde_json::Value,
    ) -> Result<StoredPlugin, OagwError> {
        let kind = PluginKind::classify(&requested_gts_id)
            .ok_or_else(|| OagwError::Validation("invalid plugin GTS identifier".to_owned()))?;
        let id = Uuid::new_v4();
        let gts_id = format!("{}~{}", PluginKind::gts_type_prefix(&kind), id);
        let stored = StoredPlugin {
            id,
            tenant_id,
            name,
            kind,
            gts_id,
            source_code,
            config_schema,
            created_at: now_secs(),
        };
        self.plugins.insert(stored.id, stored.clone());
        Ok(stored)
    }

    pub fn get_plugin(&self, tenant_id: Uuid, raw_id: &str) -> Result<StoredPlugin, OagwError> {
        let id = self.plugin_id_from_raw(tenant_id, raw_id)?;
        self.plugins
            .get(&id)
            .filter(|p| p.tenant_id == tenant_id)
            .map(|p| p.clone())
            .ok_or_else(|| OagwError::NotFound("plugin".to_owned()))
    }

    pub fn list_plugins(&self, tenant_id: Uuid, kind: Option<PluginKind>) -> Vec<StoredPlugin> {
        let mut items: Vec<StoredPlugin> = self
            .plugins
            .iter()
            .filter(|p| p.tenant_id == tenant_id && kind.map_or(true, |k| p.kind == k))
            .map(|p| p.clone())
            .collect();
        items.sort_by_key(|p| p.created_at);
        items
    }

    /// Delete a plugin. Referenced plugins return 409 PluginInUse.
    pub fn delete_plugin(&self, tenant_id: Uuid, raw_id: &str) -> Result<(), OagwError> {
        let id = self.plugin_id_from_raw(tenant_id, raw_id)?;
        let plugin = self
            .plugins
            .get(&id)
            .filter(|p| p.tenant_id == tenant_id)
            .map(|p| (*p).clone())
            .ok_or_else(|| OagwError::NotFound("plugin".to_owned()))?;

        let (upstreams, routes) = self.plugin_references(&plugin.gts_id, id.to_string());
        if upstreams > 0 || routes > 0 {
            return Err(OagwError::PluginInUse {
                plugin_id: plugin.gts_id,
                upstreams,
                routes,
            });
        }
        self.plugins.remove(&id);
        Ok(())
    }

    // -------------------------------------------------------------------
    // Control Plane: helpers
    // -------------------------------------------------------------------

    fn resolve_own_upstream(&self, tenant_id: Uuid, raw: &str) -> Result<Uuid, OagwError> {
        let id = extract_resource_uuid(raw, UPSTREAM_TYPE)
            .ok_or_else(|| OagwError::Validation("invalid upstream_id".to_owned()))?;
        self.upstreams
            .get(&id)
            .filter(|u| u.tenant_id == tenant_id)
            .map(|u| u.id)
            .ok_or_else(|| {
                OagwError::Validation(format!("upstream '{raw}' does not exist in this tenant"))
            })
    }

    fn check_route_conflict(
        &self,
        tenant_id: Uuid,
        upstream_id: Uuid,
        exclude: Option<Uuid>,
        config: &RouteConfig,
    ) -> Result<(), OagwError> {
        let Some(http) = config.http_match() else {
            return Ok(());
        };
        let new_path = normalize_path(&http.path);
        for route in self.routes.iter() {
            if route.tenant_id != tenant_id
                || route.upstream_id != upstream_id
                || Some(route.id) == exclude
            {
                continue;
            }
            let Some(other) = route.config.http_match() else {
                continue;
            };
            let same_path = normalize_path(&other.path) == new_path;
            let same_method = methods_overlap(&other.methods, &http.methods);
            if same_path && same_method {
                return Err(OagwError::Validation(format!(
                    "route conflict: an enabled route under this upstream already matches \
                     path '{}' for the same method set",
                    http.path
                )));
            }
        }
        Ok(())
    }

    fn plugin_id_from_raw(&self, tenant_id: Uuid, raw: &str) -> Result<Uuid, OagwError> {
        let id = raw
            .parse::<Uuid>()
            .ok()
            .or_else(|| crate::domain::ids::gts_instance(raw).and_then(|i| i.parse().ok()))
            .ok_or_else(|| OagwError::NotFound("plugin".to_owned()))?;
        let known = self
            .plugins
            .get(&id)
            .filter(|p| p.tenant_id == tenant_id)
            .is_some();
        if known {
            Ok(id)
        } else {
            Err(OagwError::NotFound("plugin".to_owned()))
        }
    }

    /// Count upstream/route bindings referencing a plugin GTS id or UUID.
    fn plugin_references(&self, gts_id: &str, uuid_string: String) -> (usize, usize) {
        let refers = |binding: &PluginBinding| {
            let r = binding.plugin_ref();
            r == gts_id || r == uuid_string
        };
        let mut upstreams = 0usize;
        let mut routes = 0usize;
        for u in self.upstreams.iter() {
            let in_plugins = u.config.plugins.items.iter().any(refers);
            let in_auth = u
                .config
                .auth
                .plugin_type
                .as_deref()
                .map_or(false, |t| t == gts_id || t == uuid_string);
            if in_plugins || in_auth {
                upstreams += 1;
            }
        }
        for r in self.routes.iter() {
            if r.config.plugins.items.iter().any(refers) {
                routes += 1;
            }
        }
        (upstreams, routes)
    }

    // -------------------------------------------------------------------
    // Data Plane: proxy
    // -------------------------------------------------------------------

    /// Run the full proxy pipeline for one request.
    ///
    /// `alias` is the routing alias; `req_path` is everything after the
    /// alias segment (`"/suffix?query"`, `"?query"` or empty).
    #[allow(clippy::too_many_arguments)]
    pub async fn proxy(
        &self,
        security: &SecurityContext,
        alias: &str,
        req_path: &str,
        method: Method,
        mut inbound_headers: HeaderMap,
        body: Option<Bytes>,
    ) -> Result<ProxyOutcome, OagwError> {
        let alias = crate::domain::alias::normalize(alias);

        // 1. Resolve alias → enabled upstream.
        let upstream = self
            .get_upstream_by_alias(security.subject_tenant_id(), &alias)
            .ok_or_else(|| OagwError::RouteNotFound(alias.clone()))?;
        if !upstream.config.enabled {
            return Err(OagwError::LinkUnavailable(format!(
                "upstream '{}' is disabled",
                upstream.alias
            )));
        }

        // Split path / query.
        let (path_suffix, query_str) = req_path
            .split_once('?')
            .map_or((req_path, ""), |(p, q)| (p, q));
        let request_path = normalize_path(path_suffix);
        let inbound_query: Vec<(String, String)> = form_urlencoded::parse(query_str.as_bytes())
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();

        // 2. Endpoint selection (X-OAGW-Target-Host matrix).
        let endpoint = self.select_endpoint(&upstream, &mut inbound_headers)?;

        // Body + header validation.
        validate_request_body(&inbound_headers, body.as_ref(), self.max_body_bytes())?;

        // 3. Route match.
        let Some(route) = self.match_route(security, &upstream, &request_path, &method)? else {
            return Err(OagwError::RouteNotFound(upstream.alias.clone()));
        };

        // 4. Effective configs (route overrides upstream where stricter).
        let effective_rate = effective_rate_limit(
            [
                upstream.config.rate_limit.as_ref(),
                route.config.rate_limit.as_ref(),
            ]
            .into_iter()
            .flatten(),
        );
        let effective_cors = route.config.cors.as_ref().or(upstream.config.cors.as_ref());

        // 5. CORS actual-request validation.
        if let Some(cors) = effective_cors.filter(|c| c.enabled) {
            self.check_cors_actual(cors, &method, &inbound_headers)?;
        }

        // 6. Rate limiting.
        let rate_decision =
            self.check_rate_limit(security, &upstream, &route, effective_rate.as_ref())?;
        let show_rate_headers = effective_rate
            .as_ref()
            .map_or(false, |r| r.response_headers);

        // 7. Plugin chain execution (ADR-0002 order):
        //    Auth → Guards → Transform(on_request).
        let mut auth_headers = HeaderMap::new();
        let mut query_params: Vec<(String, String)> = Vec::new();
        self.run_auth_plugins(security, &upstream, &mut auth_headers, &mut query_params)
            .await?;
        self.run_guards(security, &upstream, &route, false, &inbound_headers)
            .await?;
        let mut transform_headers = HeaderMap::new();
        let request_chain = self.combined_chain(&upstream, &route);
        self.run_request_transforms(&request_chain, &mut transform_headers)
            .await?;

        // 8. Assemble outbound request.
        let upstream_host = endpoint.host.clone();
        let outbound_headers = self.build_outbound_headers(
            &upstream,
            &inbound_headers,
            &auth_headers,
            &transform_headers,
            &upstream_host,
        );

        // Query: allowlisted inbound query + plugin-added query params.
        let mut out_query: Vec<(String, String)> = Vec::new();
        self.filter_query(&route, &inbound_query, &mut out_query)?;
        out_query.extend(query_params);

        let url = build_url(
            &endpoint.scheme,
            &endpoint.host,
            endpoint.port,
            &request_path,
            &out_query,
        )
        .map_err(OagwError::Validation)?;

        let response = forward(
            &self.client,
            OutboundRequest {
                method,
                url,
                headers: outbound_headers.clone(),
                body,
            },
            self.proxy_timeout(),
            self.max_body_bytes(),
        )
        .await
        .map_err(forward_error_to_oagw)?;

        // 9. Response phase: guards + transforms, header sanitation,
        //    rate-limit headers, error-source tagging.
        let resp_headers = sanitize_inbound_headers(response.headers.clone());
        let status = StatusCode::from_u16(response.status).unwrap_or(StatusCode::BAD_GATEWAY);

        // Request-ID-style propagation: seed response-transform context with
        // the outbound request headers so transformers can read request
        // state (e.g. the injected X-Request-ID).
        let mut response_ctx = TransformContext {
            config: serde_json::Value::Null,
            headers: resp_headers,
        };
        for (name, value) in outbound_headers.iter() {
            if !response_ctx.headers.contains_key(name) {
                // Only propagate a few known propagation-eligible headers to
                // avoid leaking arbitrary request headers downstream.
                if name == "x-request-id" || name == "correlation-id" {
                    response_ctx.headers.insert(name.clone(), value.clone());
                }
            }
        }
        self.run_guards(security, &upstream, &route, true, &response_ctx.headers)
            .await?;
        self.run_response_transforms(&request_chain, &mut response_ctx.headers)
            .await?;
        self.apply_response_header_rules(&upstream, &mut response_ctx.headers);
        let mut out_headers = response_ctx.headers;
        out_headers.insert(
            crate::error::ERROR_SOURCE_HEADER,
            HeaderValue::from_static(crate::error::ERROR_SOURCE_UPSTREAM),
        );
        if show_rate_headers {
            for (name, value) in upstream_rate_limit_headers(rate_decision.as_ref()) {
                if let (Ok(name), Ok(value)) = (
                    HeaderName::from_bytes(name.as_bytes()),
                    HeaderValue::from_str(&value),
                ) {
                    out_headers.insert(name, value);
                }
            }
        }

        Ok(ProxyOutcome {
            status,
            headers: out_headers,
            body: response.body,
        })
    }

    /// Select the target endpoint per the X-OAGW-Target-Host matrix.
    fn select_endpoint(
        &self,
        upstream: &StoredUpstream,
        inbound_headers: &mut HeaderMap,
    ) -> Result<Endpoint, OagwError> {
        let endpoints = &upstream.config.server.endpoints;
        if endpoints.is_empty() {
            return Err(OagwError::LinkUnavailable(format!(
                "upstream '{}' has no endpoints",
                upstream.alias
            )));
        }

        if let Some(value) = inbound_headers.remove("x-oagw-target-host") {
            let host = match value.to_str() {
                Ok(v) => v.trim(),
                Err(_) => return Err(OagwError::InvalidTargetHost("non-ASCII".to_owned())),
            };
            if !valid_target_host(host) {
                return Err(OagwError::InvalidTargetHost(host.to_owned()));
            }
            let matched = endpoints.iter().find(|e| e.host.eq_ignore_ascii_case(host));
            return matched
                .cloned()
                .ok_or_else(|| OagwError::UnknownTargetHost(host.to_owned()));
        }

        if endpoints.len() == 1 {
            return Ok(endpoints[0].clone());
        }
        if upstream.alias_derived {
            // Multi-endpoint common-suffix alias: header required.
            return Err(OagwError::MissingTargetHost);
        }
        // Explicit-alias pools: round-robin.
        let idx = ROUND_ROBIN.fetch_add(1, Ordering::Relaxed) as usize % endpoints.len();
        Ok(endpoints[idx].clone())
    }

    /// Find the best enabled route: longest path prefix + method allowlist +
    /// path-suffix mode.
    fn match_route(
        &self,
        security: &SecurityContext,
        upstream: &StoredUpstream,
        request_path: &str,
        method: &Method,
    ) -> Result<Option<StoredRoute>, OagwError> {
        let tenant = security.subject_tenant_id();
        let method_str = crate::domain::plugin::method_to_str(method).to_ascii_uppercase();

        let mut best: Option<(usize, Uuid)> = None;
        for route in self.routes.iter() {
            if route.tenant_id != tenant
                || route.upstream_id != upstream.id
                || !route.config.enabled
            {
                continue;
            }
            let Some(http) = route.config.http_match() else {
                continue;
            };
            if !http.methods.is_empty()
                && !http
                    .methods
                    .iter()
                    .any(|m| m.eq_ignore_ascii_case(&method_str))
            {
                continue;
            }
            let route_path = normalize_path(&http.path);
            if !path_prefix_matches(request_path, &route_path) {
                continue;
            }
            match best {
                Some((best_len, _)) if route_path.len() <= best_len => {}
                _ => best = Some((route_path.len(), route.id)),
            }
        }

        let Some((_, id)) = best else {
            return Ok(None);
        };
        let route = self
            .routes
            .get(&id)
            .map(|r| r.clone())
            .ok_or_else(|| OagwError::RouteNotFound(upstream.alias.clone()))?;
        let http = route.config.http_match().expect("matched route is http");

        // Path-suffix mode.
        let suffix = &request_path[normalize_path(&http.path).len()..];
        if !suffix.is_empty() && http.path_suffix_mode.eq_ignore_ascii_case("disabled") {
            return Err(OagwError::Validation(format!(
                "path suffix '{}' is not allowed for route '{}' (path_suffix_mode: disabled)",
                http.path, suffix
            )));
        }
        Ok(Some(route))
    }

    /// Filter inbound query params against the route allowlist. Empty
    /// allowlist ⇒ nothing forwarded; non-empty ⇒ unknown params rejected.
    fn filter_query(
        &self,
        route: &StoredRoute,
        inbound: &[(String, String)],
        out: &mut Vec<(String, String)>,
    ) -> Result<(), OagwError> {
        let Some(http) = route.config.http_match() else {
            return Ok(());
        };
        if http.query_allowlist.is_empty() {
            return Ok(()); // "empty ⇒ none forwarded"
        }
        for (k, v) in inbound {
            if http.query_allowlist.iter().any(|a| a == k) {
                out.push((k.clone(), v.clone()));
            } else {
                return Err(OagwError::Validation(format!(
                    "query parameter '{k}' is not allowlisted for this route"
                )));
            }
        }
        Ok(())
    }

    /// CORS actual-request validation (403 on origin/method mismatch).
    fn check_cors_actual(
        &self,
        cors: &CorsConfig,
        method: &Method,
        headers: &HeaderMap,
    ) -> Result<(), OagwError> {
        let Some(origin) = headers.get("origin").and_then(|v| v.to_str().ok()) else {
            return Ok(()); // same-origin / no origin: CORS not applicable
        };
        if !cors.is_origin_allowed(origin) {
            return Err(OagwError::CorsOriginNotAllowed(format!(
                "origin '{origin}' is not in allowed_origins {:?}",
                cors.allowed_origins
            )));
        }
        if !cors.is_method_allowed(&method.to_string()) {
            return Err(OagwError::CorsMethodNotAllowed(format!(
                "method '{}' is not in allowed_methods {:?}",
                method, cors.allowed_methods
            )));
        }
        Ok(())
    }

    /// Rate limiting via the effective config (strictest wins).
    fn check_rate_limit(
        &self,
        security: &SecurityContext,
        upstream: &StoredUpstream,
        route: &StoredRoute,
        effective: Option<&RateLimitConfig>,
    ) -> Result<Option<RateLimitDecision>, OagwError> {
        let Some(cfg) = effective else {
            return Ok(None);
        };
        let Some(sustained) = &cfg.sustained else {
            return Ok(None);
        };
        let capacity = cfg.burst.as_ref().map_or(sustained.rate, |b| b.capacity);

        let scope = cfg.scope.as_deref().unwrap_or("tenant");
        let scope_key = match scope {
            "global" => "global".to_owned(),
            "user" => security.subject_id().to_string(),
            "ip" => "ip".to_owned(), // no client IP plumbed through; per-IP requires the proxy layer
            "route" => route.id.to_string(),
            _ => security.subject_tenant_id().to_string(),
        };
        let key = format!("rl:{}:{}:{}", scope_key, upstream.id, route.id);

        let strategy = match cfg.strategy.as_deref().unwrap_or("reject") {
            "queue" => "degrade", // local queueing is not implementable: admit
            other => other,
        };

        let decision = self.limiter.check(
            &key,
            sustained.rate,
            &sustained.window,
            capacity,
            cfg.cost.max(1),
            strategy,
        );
        if !decision.allowed {
            return Err(OagwError::RateLimitExceeded {
                retry_after_secs: decision.retry_after_secs,
            });
        }
        Ok(Some(decision))
    }

    /// Run the upstream auth plugin; injects credentials via header/query.
    async fn run_auth_plugins(
        &self,
        security: &SecurityContext,
        upstream: &StoredUpstream,
        auth_headers: &mut HeaderMap,
        query_params: &mut Vec<(String, String)>,
    ) -> Result<(), OagwError> {
        let Some(plugin_type) = &upstream.config.auth.plugin_type else {
            return Ok(());
        };
        let plugin = self
            .registry
            .resolve_auth(plugin_type)
            .ok_or_else(|| OagwError::PluginNotFound(plugin_type.clone()))?;

        let credstore = self.client_hub_credstore().map_err(OagwError::Internal)?;

        let mut ctx = AuthContext {
            config: upstream.config.auth.config.clone(),
            headers: std::mem::take(auth_headers),
            query_params: std::mem::take(query_params),
            security_context: security.clone(),
            credstore,
        };
        let result = plugin.authenticate(&mut ctx).await;
        *auth_headers = ctx.headers;
        *query_params = ctx.query_params;
        result.map_err(|e| self.map_plugin_error(plugin_type, e))
    }

    /// Run guards for one phase (upstream chain then route chain).
    async fn run_guards(
        &self,
        _security: &SecurityContext,
        upstream: &StoredUpstream,
        route: &StoredRoute,
        is_response: bool,
        headers: &HeaderMap,
    ) -> Result<(), OagwError> {
        for chain in [&upstream.config.plugins, &route.config.plugins] {
            for binding in &chain.items {
                let plugin_ref = binding.plugin_ref().to_owned();
                if PluginKind::classify(&plugin_ref) != Some(PluginKind::Guard) {
                    continue;
                }
                let plugin = self
                    .registry
                    .resolve_guard(&plugin_ref)
                    .ok_or_else(|| OagwError::PluginNotFound(plugin_ref.clone()))?;
                let ctx = GuardContext {
                    headers: headers.clone(),
                    is_response,
                    config: binding.config(),
                };
                let result = if is_response {
                    plugin.guard_response(&ctx).await
                } else {
                    plugin.guard_request(&ctx).await
                };
                result.map_err(|e| self.map_plugin_error(&plugin_ref, e))?;
            }
        }
        Ok(())
    }

    /// Combined transform chain: upstream plugins before route plugins.
    fn combined_chain(&self, upstream: &StoredUpstream, route: &StoredRoute) -> PluginChainConfig {
        let mut items = upstream.config.plugins.items.clone();
        items.extend(route.config.plugins.items.clone());
        PluginChainConfig {
            sharing: None,
            items,
        }
    }

    async fn run_request_transforms(
        &self,
        chain: &PluginChainConfig,
        headers: &mut HeaderMap,
    ) -> Result<(), OagwError> {
        for binding in &chain.items {
            let plugin_ref = binding.plugin_ref().to_owned();
            if PluginKind::classify(&plugin_ref) != Some(PluginKind::Transform) {
                continue;
            }
            let plugin = self
                .registry
                .resolve_transform(&plugin_ref)
                .ok_or_else(|| OagwError::PluginNotFound(plugin_ref.clone()))?;
            let mut ctx = TransformContext {
                config: binding.config(),
                headers: std::mem::take(headers),
            };
            let result = plugin.transform_request(&mut ctx).await;
            *headers = ctx.headers;
            result.map_err(|e| self.map_plugin_error(&plugin_ref, e))?;
        }
        Ok(())
    }

    async fn run_response_transforms(
        &self,
        chain: &PluginChainConfig,
        headers: &mut HeaderMap,
    ) -> Result<(), OagwError> {
        for binding in &chain.items {
            let plugin_ref = binding.plugin_ref().to_owned();
            if PluginKind::classify(&plugin_ref) != Some(PluginKind::Transform) {
                continue;
            }
            let plugin = self
                .registry
                .resolve_transform(&plugin_ref)
                .ok_or_else(|| OagwError::PluginNotFound(plugin_ref.clone()))?;
            let mut ctx = TransformContext {
                config: binding.config(),
                headers: std::mem::take(headers),
            };
            let result = plugin.transform_response(&mut ctx).await;
            *headers = ctx.headers;
            result.map_err(|e| self.map_plugin_error(&plugin_ref, e))?;
        }
        Ok(())
    }

    fn apply_response_header_rules(&self, upstream: &StoredUpstream, headers: &mut HeaderMap) {
        let h = &upstream.config.headers.response;
        apply_header_rules(headers, &h.set, &h.add, &h.remove);
    }

    /// Assemble outbound headers: passthrough policy + transforms on the
    /// inbound set, then layer auth/transform-produced headers, then strip
    /// hop-by-hop and set `Host`.
    fn build_outbound_headers(
        &self,
        upstream: &StoredUpstream,
        inbound: &HeaderMap,
        auth_headers: &HeaderMap,
        transform_headers: &HeaderMap,
        upstream_host: &str,
    ) -> HeaderMap {
        let mut out = HeaderMap::new();
        let h = &upstream.config.headers.request;
        match h.passthrough.as_deref().unwrap_or("none") {
            "all" => {
                for (k, v) in inbound {
                    out.insert(k.clone(), v.clone());
                }
            }
            "allowlist" => {
                for (k, v) in inbound {
                    if h.passthrough_allowlist
                        .iter()
                        .any(|a| a.eq_ignore_ascii_case(k.as_str()))
                    {
                        out.insert(k.clone(), v.clone());
                    }
                }
            }
            _ => {}
        }
        apply_header_rules(&mut out, &h.set, &h.add, &h.remove);
        for (k, v) in auth_headers {
            out.append(k.clone(), v.clone());
        }
        for (k, v) in transform_headers {
            out.append(k.clone(), v.clone());
        }
        sanitize_outbound_headers(out, upstream_host)
    }

    fn map_plugin_error(&self, plugin_ref: &str, e: PluginError) -> OagwError {
        match &e {
            PluginError::Internal(msg)
                if msg.contains("credential store returned no secret")
                    || msg.contains("cred_store lookup failed")
                    || msg.contains("invalid secret_ref") =>
            {
                OagwError::SecretNotFound(msg.clone())
            }
            PluginError::Internal(msg) => {
                OagwError::Internal(format!("plugin '{plugin_ref}' failed: {msg}"))
            }
            PluginError::Unknown(msg) => OagwError::PluginNotFound(msg.clone()),
            _ => e.into_oagw_error(plugin_ref),
        }
    }

    /// Lazily resolve the shared credstore client from the toolkit client
    /// hub (registered by the credstore gear).
    fn client_hub_credstore(&self) -> Result<Arc<dyn credstore_sdk::CredStoreClientV1>, String> {
        let hub =
            self.client_hub.lock().clone().ok_or_else(|| {
                "credstore resolver unavailable: no client hub attached".to_owned()
            })?;
        hub.get::<dyn credstore_sdk::CredStoreClientV1>()
            .map_err(|e| format!("credstore resolver unavailable: {e}"))
    }
}

// ---------------------------------------------------------------------------
// Free helpers
// ---------------------------------------------------------------------------

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Validate base upstream config before storage.
fn validate_upstream_config(config: &UpstreamConfig) -> Result<(), OagwError> {
    if config.server.endpoints.is_empty() {
        return Err(OagwError::Validation(
            "upstream must declare at least one endpoint".to_owned(),
        ));
    }
    // Uniform scheme/port across the pool (DESIGN "Multi-Endpoint").
    let first = &config.server.endpoints[0];
    for ep in &config.server.endpoints[1..] {
        if !ep.scheme.eq_ignore_ascii_case(&first.scheme) || ep.port != first.port {
            return Err(OagwError::Validation(
                "all endpoints must share the same scheme and port".to_owned(),
            ));
        }
    }
    Ok(())
}

fn validate_route_config(config: &RouteConfig) -> Result<(), OagwError> {
    if let Some(http) = config.http_match() {
        if !http.path.starts_with('/') {
            return Err(OagwError::Validation(
                "route path must start with '/'".to_owned(),
            ));
        }
    }
    Ok(())
}

/// Normalize a request/route path to a canonical absolute form: must start
/// with `/`, trailing slashes stripped (except the root `/`).
fn normalize_path(path: &str) -> String {
    let p = path.trim();
    let owned;
    let p = if p.starts_with('/') {
        p
    } else {
        owned = format!("/{p}");
        &owned
    };
    let p = p.trim_end_matches('/');
    if p.is_empty() {
        "/".to_owned()
    } else {
        p.to_owned()
    }
}

/// True when `request` equals `route` or has `route` as a path-segment
/// prefix. The route `/` matches everything.
fn path_prefix_matches(request: &str, route: &str) -> bool {
    if route == "/" {
        return true;
    }
    if request == route {
        return true;
    }
    request
        .strip_prefix(route)
        .map_or(false, |rest| rest.starts_with('/'))
}

/// Two method sets overlap (empty set = any method).
fn methods_overlap(a: &[String], b: &[String]) -> bool {
    if a.is_empty() || b.is_empty() {
        return true;
    }
    b.iter()
        .any(|m| a.iter().any(|x| x.eq_ignore_ascii_case(m)))
}

/// `X-OAGW-Target-Host` format rule: hostname or IP only — no port, path,
/// scheme, or special characters.
fn valid_target_host(host: &str) -> bool {
    !host.is_empty()
        && host.len() <= 253
        && !host.contains(':')
        && !host.contains('/')
        && !host.contains('?')
        && !host.contains('#')
        && !host.contains('@')
        && !host.contains(' ')
        && host
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'.' || b == b'_')
        && (host.parse::<std::net::IpAddr>().is_ok()
            || crate::domain::alias::validate_hostname(host.strip_suffix('.').unwrap_or(host))
                .is_ok())
}

/// Validate request body constraints: Transfer-Encoding support, valid
/// Content-Length matching the buffered body, hard size cap.
fn validate_request_body(
    headers: &HeaderMap,
    body: Option<&Bytes>,
    max_body_bytes: usize,
) -> Result<(), OagwError> {
    if let Some(te) = headers.get("transfer-encoding") {
        let value = te.to_str().unwrap_or("");
        if !value.eq_ignore_ascii_case("chunked") {
            return Err(OagwError::Validation(format!(
                "unsupported Transfer-Encoding '{value}' (only chunked is supported)"
            )));
        }
    }
    if let Some(cl) = headers.get("content-length") {
        let value = cl.to_str().unwrap_or("");
        let parsed: u64 = value
            .parse()
            .map_err(|_| OagwError::Validation(format!("invalid Content-Length '{value}'")))?;
        if parsed > max_body_bytes as u64 {
            return Err(OagwError::PayloadTooLarge);
        }
        if let Some(body) = body {
            if parsed != body.len() as u64 {
                return Err(OagwError::Validation(format!(
                    "Content-Length {parsed} does not match body size {}",
                    body.len()
                )));
            }
        }
    }
    Ok(())
}

/// Apply header transformation rules: remove, set (overwrite), add
/// (append).
fn apply_header_rules(
    headers: &mut HeaderMap,
    set: &std::collections::HashMap<String, String>,
    add: &std::collections::HashMap<String, String>,
    remove: &[String],
) {
    for name in remove {
        if let Ok(name) = axum::http::header::HeaderName::from_bytes(name.as_bytes()) {
            headers.remove(name);
        }
    }
    for (name, value) in set {
        if let (Ok(name), Ok(value)) = (
            axum::http::header::HeaderName::from_bytes(name.as_bytes()),
            HeaderValue::from_str(value),
        ) {
            headers.insert(name, value);
        }
    }
    for (name, value) in add {
        if let (Ok(name), Ok(value)) = (
            axum::http::header::HeaderName::from_bytes(name.as_bytes()),
            HeaderValue::from_str(value),
        ) {
            headers.append(name, value);
        }
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    fn test_service() -> OagwService {
        let client = HttpClient::builder().build().expect("client");
        OagwService::new(OagwConfig::default(), client)
    }

    fn http_upstream(alias: Option<&str>, host: &str) -> UpstreamConfig {
        UpstreamConfig {
            enabled: true,
            alias: alias.map(str::to_owned),
            tags: vec![],
            server: crate::domain::model::ServerConfig {
                endpoints: vec![Endpoint {
                    scheme: "https".into(),
                    host: host.into(),
                    port: None,
                }],
            },
            protocol: crate::domain::model::protocols::HTTP.to_owned(),
            auth: Default::default(),
            headers: Default::default(),
            plugins: Default::default(),
            rate_limit: None,
            cors: None,
        }
    }

    fn route_for(upstream_id: Uuid, path: &str, methods: &[&str]) -> RouteConfig {
        RouteConfig {
            upstream_id: upstream_id.to_string(),
            match_config: crate::domain::model::MatchConfig {
                http: Some(crate::domain::model::HttpMatch {
                    methods: methods.iter().map(|m| m.to_string()).collect(),
                    path: path.to_owned(),
                    query_allowlist: vec![],
                    path_suffix_mode: "append".to_owned(),
                }),
                grpc: None,
            },
            plugins: Default::default(),
            rate_limit: None,
            cors: None,
            tags: vec![],
            enabled: true,
        }
    }

    #[tokio::test]
    async fn create_upstream_derives_alias() {
        let service = test_service();
        let tenant = Uuid::new_v4();
        let stored = service
            .create_upstream(tenant, http_upstream(None, "api.openai.com"))
            .unwrap();
        assert_eq!(stored.alias, "api.openai.com");
        assert!(stored.alias_derived);
    }

    #[tokio::test]
    async fn create_upstream_alias_conflict_409() {
        let service = test_service();
        let tenant = Uuid::new_v4();
        service
            .create_upstream(tenant, http_upstream(None, "api.openai.com"))
            .unwrap();
        let err = service
            .create_upstream(tenant, http_upstream(None, "api.openai.com"))
            .unwrap_err();
        assert!(matches!(err, OagwError::Validation(_)));
    }

    #[tokio::test]
    async fn update_upstream_alias_immutable() {
        let service = test_service();
        let tenant = Uuid::new_v4();
        let stored = service
            .create_upstream(tenant, http_upstream(None, "api.openai.com"))
            .unwrap();
        // Changing hostname changes the derived alias → rejected.
        let changed = http_upstream(None, "eu.openai.com");
        let err = service
            .update_upstream(tenant, &stored.id.to_string(), changed)
            .unwrap_err();
        assert!(matches!(err, OagwError::Validation(_)));
        // Same endpoints → allowed.
        let same = http_upstream(None, "api.openai.com");
        assert!(
            service
                .update_upstream(tenant, &stored.id.to_string(), same)
                .is_ok()
        );
    }

    #[tokio::test]
    async fn routes_scoped_and_conflict_detected() {
        let service = test_service();
        let tenant = Uuid::new_v4();
        let upstream = service
            .create_upstream(tenant, http_upstream(None, "api.openai.com"))
            .unwrap();

        let r1 = service
            .create_route(tenant, route_for(upstream.id, "/v1/models", &["GET"]))
            .unwrap();
        assert_eq!(r1.upstream_id, upstream.id);

        // Same path + overlapping method → conflict.
        let dup = route_for(upstream.id, "/v1/models", &["GET", "POST"]);
        assert!(service.create_route(tenant, dup).is_err());

        // Different path → OK.
        let r2 = service
            .create_route(tenant, route_for(upstream.id, "/v1/chat", &["POST"]))
            .unwrap();
        assert_ne!(r1.id, r2.id);
    }

    #[tokio::test]
    async fn create_route_rejects_unknown_upstream() {
        let service = test_service();
        let err = service
            .create_route(Uuid::new_v4(), route_for(Uuid::new_v4(), "/x", &["GET"]))
            .unwrap_err();
        assert!(matches!(err, OagwError::Validation(_)));
    }

    #[tokio::test]
    async fn delete_upstream_cascades_routes() {
        let service = test_service();
        let tenant = Uuid::new_v4();
        let upstream = service
            .create_upstream(tenant, http_upstream(None, "api.openai.com"))
            .unwrap();
        service
            .create_route(tenant, route_for(upstream.id, "/v1", &["GET"]))
            .unwrap();
        service
            .delete_upstream(tenant, &upstream.id.to_string())
            .unwrap();
        assert_eq!(service.route_count(), 0);
        assert_eq!(service.upstream_count(), 0);
    }

    #[tokio::test]
    async fn plugins_crud_and_in_use() {
        let service = test_service();
        let tenant = Uuid::new_v4();
        let plugin = service
            .create_plugin(
                tenant,
                "my-guard".to_owned(),
                "gts.cf.core.oagw.guard_plugin.v1~my-custom-guard.v1".to_owned(),
                "def guard(ctx): pass".to_owned(),
                serde_json::json!({}),
            )
            .unwrap();
        assert_eq!(plugin.kind, PluginKind::Guard);

        // Unreferenced → deletable.
        service
            .delete_plugin(tenant, &plugin.id.to_string())
            .unwrap();
        assert_eq!(service.plugin_count(), 0);
    }

    #[tokio::test]
    async fn target_host_validation() {
        assert!(valid_target_host("api.openai.com"));
        assert!(valid_target_host("127.0.0.1"));
        assert!(valid_target_host("10.0.1.1"));
        assert!(!valid_target_host("api.openai.com:8443"));
        assert!(!valid_target_host("/etc/passwd"));
        assert!(!valid_target_host("a b.c"));
    }

    #[tokio::test]
    async fn cors_wildcard_credentials_rejected() {
        let service = test_service();
        let tenant = Uuid::new_v4();
        let mut config = http_upstream(None, "api.openai.com");
        config.cors = Some(CorsConfig {
            enabled: true,
            allowed_origins: vec!["*".to_owned()],
            allow_credentials: true,
            ..Default::default()
        });
        assert!(service.create_upstream(tenant, config).is_err());
    }

    #[tokio::test]
    async fn effective_rate_strictest_wins() {
        // strict = 1/s on the tenant scope, loose = 5/s at the upstream level.
        let strict = RateLimitConfig {
            sustained: Some(crate::domain::model::SustainedRate {
                rate: 1,
                window: "second".into(),
            }),
            burst: Some(crate::domain::model::BurstConfig { capacity: 5 }),
            scope: Some("tenant".into()),
            ..Default::default()
        };
        let loose = RateLimitConfig {
            sustained: Some(crate::domain::model::SustainedRate {
                rate: 5,
                window: "second".into(),
            }),
            ..Default::default()
        };
        let effective = effective_rate_limit([&loose, &strict]).unwrap();
        assert_eq!(effective.sustained.unwrap().rate, 1);
    }

    #[tokio::test]
    async fn path_matching_semantics() {
        assert!(path_prefix_matches("/v1/models", "/v1/models"));
        assert!(path_prefix_matches("/v1/models/chat", "/v1/models"));
        assert!(!path_prefix_matches("/v1/modelsx", "/v1/models"));
        assert!(path_prefix_matches("/anything", "/"));
        assert!(path_prefix_matches("/", "/"));
    }
}
