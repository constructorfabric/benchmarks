//! Control-plane and data-plane services.

use std::collections::BTreeMap;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use http::{HeaderMap, Method, StatusCode};
use tenant_resolver_sdk::TenantResolverClient;
use toolkit_security::SecurityContext;

use crate::config::OagwConfig;
use crate::domain::alias::normalize;
use crate::domain::error::{ErrorKind, OagwError};
use crate::domain::matcher::{self, SelectedRoute};
use crate::domain::model::{
    AuthConfig, Burst, CorsConfig, PluginBindingDto, PluginsConfig, RateLimitConfig, Route,
    SharingMode, Upstream,
};
use crate::domain::plugin::{GuardDecision, RequestContext, ResponseContext};
use crate::domain::ratelimit::{RateDecision, RateLimiter, scope_key};
use crate::domain::validation::{self, merge_plugins};
use crate::infra::proxy::{
    OutboundRequest, ProxyEngine, apply_response_headers, build_outbound_headers, is_streaming,
    mark_upstream_source, restore_upgrade_headers, select_endpoint, strip_hop_by_hop,
};
use crate::infra::store::Store;
use crate::infra::tenant::tenant_chain;

/// Alias for a custom plugin definition stored in the store.
pub type StoredPlugin = crate::domain::model::CustomPlugin;

/// The miss reported for an alias no tenant in the walked scope owns.
///
/// A nil tenant owns nothing and has no ancestors, so its chain can never hold
/// an upstream: the miss is reported exactly as the empty-candidates miss
/// instead of being masked by the `tenant not found` fault a nil scope raises
/// in the resolver (which would surface as a protocol error, not a miss).
fn alias_miss(normalized_alias: &str) -> OagwError {
    OagwError::new(
        ErrorKind::RouteNotFound,
        format!("no upstream is registered for alias '{normalized_alias}'"),
    )
}

/// A route or upstream plugin binding prepared for execution.
#[derive(Debug, Clone)]
pub struct Binding {
    /// Canonical plugin identifier.
    pub plugin_type: String,
    /// Binding configuration object.
    pub config: serde_json::Value,
}

/// Effective configuration for one proxied request: the resolved upstream
/// merged with its matching route and any `enforce`-mode ancestor values.
#[derive(Debug, Clone)]
pub struct EffectiveConfig {
    /// Upstream the request is dialed against.
    pub upstream: Arc<Upstream>,
    /// Matched route, when one matched.
    pub route: Option<SelectedRoute>,
    /// Rate limits to enforce, most specific last.
    pub rate_limits: Vec<(String, RateLimitConfig)>,
    /// Effective CORS configuration.
    pub cors: Option<CorsConfig>,
    /// Effective header rules.
    pub headers: Option<crate::domain::model::HeadersConfig>,
    /// Plugin bindings: upstream chain first, then route chain.
    pub plugins: Vec<Binding>,
    /// Effective auth binding.
    pub auth: Option<AuthConfig>,
    /// Tenants whose rate limit contributed to the effective one.
    pub enforcing_tenants: Vec<uuid::Uuid>,
}

/// The rate limits a relay enforced, most specific last, with the decision each
/// one reached.
type EnforcedLimits = Vec<(String, RateLimitConfig, RateDecision)>;

/// Control plane: tenant-scoped CRUD over the authoritative in-process store.
pub struct ControlPlaneService {
    store: Arc<Store>,
    tenant_resolver: Arc<dyn TenantResolverClient>,
    auth_plugins: Arc<crate::domain::plugin::AuthPluginRegistry>,
    guards: Arc<crate::domain::plugin::GuardPluginRegistry>,
    transforms: Arc<crate::domain::plugin::TransformPluginRegistry>,
}

impl ControlPlaneService {
    /// Assemble the control plane.
    #[must_use]
    pub fn new(
        store: Arc<Store>,
        tenant_resolver: Arc<dyn TenantResolverClient>,
        auth_plugins: Arc<crate::domain::plugin::AuthPluginRegistry>,
        guards: Arc<crate::domain::plugin::GuardPluginRegistry>,
        transforms: Arc<crate::domain::plugin::TransformPluginRegistry>,
    ) -> Self {
        Self {
            store,
            tenant_resolver,
            auth_plugins,
            guards,
            transforms,
        }
    }

    /// The authoritative store.
    #[must_use]
    pub fn store(&self) -> &Arc<Store> {
        &self.store
    }

    /// Registry of built-in auth plugins.
    #[must_use]
    pub fn auth_plugins(&self) -> &crate::domain::plugin::AuthPluginRegistry {
        &self.auth_plugins
    }

    /// Registry of built-in guard plugins.
    #[must_use]
    pub fn guards(&self) -> &crate::domain::plugin::GuardPluginRegistry {
        &self.guards
    }

    /// Registry of built-in transform plugins.
    #[must_use]
    pub fn transforms(&self) -> &crate::domain::plugin::TransformPluginRegistry {
        &self.transforms
    }

    /// The plugin catalogue: built-in plugins plus tenant-defined plugins.
    #[must_use]
    pub fn plugin_catalogue(&self, tenant: uuid::Uuid) -> Vec<crate::domain::model::CustomPlugin> {
        let mut entries = Vec::new();
        for identifier in crate::infra::plugin::catalogue::BUILTIN_PLUGIN_IDS {
            entries.push(crate::infra::plugin::catalogue::builtin_definition(
                identifier,
            ));
        }
        entries.extend(
            self.store
                .plugins_of(tenant)
                .iter()
                .map(Arc::as_ref)
                .cloned(),
        );
        entries
    }

    /// Resolve a custom plugin by UUID, tenant-scoped.
    #[must_use]
    pub fn get_plugin(&self, tenant: uuid::Uuid, id: uuid::Uuid) -> Option<Arc<StoredPlugin>> {
        self.store.get_plugin(tenant, id)
    }

    /// Insert a custom plugin definition.
    ///
    /// # Errors
    ///
    /// Returns a validation error when the definition is malformed, and a
    /// conflict error when the tenant already owns a definition of that name.
    pub fn insert_plugin(&self, plugin: StoredPlugin) -> Result<Arc<StoredPlugin>, OagwError> {
        self.store.insert_plugin(plugin)
    }

    /// The tenant chain for `tenant_id`, descendant first.
    ///
    /// # Errors
    ///
    /// Returns an error when the tenant resolver is unavailable.
    pub async fn tenant_chain(
        &self,
        ctx: &SecurityContext,
        tenant_id: uuid::Uuid,
    ) -> Result<Vec<uuid::Uuid>, OagwError> {
        tenant_chain(&self.tenant_resolver, ctx, tenant_id)
            .await
            .map_err(|err| OagwError::new(ErrorKind::ProtocolError, err.to_string()))
    }

    /// Resolve the closest upstream for `alias`, walking the tenant chain.
    ///
    /// # Errors
    ///
    /// Returns `route.not_found` when no tenant in the chain owns the alias.
    pub async fn resolve_upstream(
        &self,
        ctx: &SecurityContext,
        tenant_id: uuid::Uuid,
        alias: &str,
    ) -> Result<(Vec<uuid::Uuid>, Vec<Arc<Upstream>>), OagwError> {
        let normalized = normalize(alias);
        if tenant_id.is_nil() {
            return Err(alias_miss(&normalized));
        }
        let chain = self.tenant_chain(ctx, tenant_id).await?;
        let mut candidates = Vec::new();
        for tenant in &chain {
            if let Some(upstream) = self.store.upstream_by_alias(*tenant, &normalized) {
                candidates.push(upstream);
            }
        }
        if candidates.is_empty() {
            return Err(alias_miss(&normalized));
        }
        Ok((chain, candidates))
    }
}

/// Data plane: resolves configuration and executes the plugin chain.
pub struct DataPlaneService {
    config: OagwConfig,
    store: Arc<Store>,
    engine: Arc<ProxyEngine>,
    limiter: Arc<RateLimiter>,
    auth_plugins: Arc<crate::domain::plugin::AuthPluginRegistry>,
    guards: Arc<crate::domain::plugin::GuardPluginRegistry>,
    transforms: Arc<crate::domain::plugin::TransformPluginRegistry>,
    tenant_resolver: Arc<dyn TenantResolverClient>,
}

impl DataPlaneService {
    /// Assemble the data plane.
    ///
    /// The service owns its rate limiter: it is not shared with anything else,
    /// and [`Self::limiter`] hands it out for observation.
    #[must_use]
    pub fn new(
        config: OagwConfig,
        store: Arc<Store>,
        engine: Arc<ProxyEngine>,
        auth_plugins: Arc<crate::domain::plugin::AuthPluginRegistry>,
        guards: Arc<crate::domain::plugin::GuardPluginRegistry>,
        transforms: Arc<crate::domain::plugin::TransformPluginRegistry>,
        tenant_resolver: Arc<dyn TenantResolverClient>,
    ) -> Self {
        let limiter = Arc::new(RateLimiter::new());
        // The limiter owns buckets the store cannot reach; a deleted upstream
        // (and its routes) must not leave them behind holding memory for keys
        // nothing can request again.
        store.on_upstream_removed({
            let limiter = Arc::clone(&limiter);
            Arc::new(move |upstream_id, route_ids| {
                limiter.drop_upstream(upstream_id, route_ids);
            })
        });
        Self {
            config,
            store,
            engine,
            limiter,
            auth_plugins,
            guards,
            transforms,
            tenant_resolver,
        }
    }

    /// The process-local rate-limit buckets.
    #[must_use]
    pub fn limiter(&self) -> &Arc<RateLimiter> {
        &self.limiter
    }

    /// The gear configuration in force.
    #[must_use]
    pub fn config(&self) -> &OagwConfig {
        &self.config
    }

    /// The authoritative store.
    #[must_use]
    pub fn store(&self) -> &Arc<Store> {
        &self.store
    }

    /// Registry of built-in transform plugins.
    #[must_use]
    pub fn transforms(&self) -> &crate::domain::plugin::TransformPluginRegistry {
        &self.transforms
    }

    /// The round-robin cursor for an upstream.
    #[must_use]
    pub fn next_round_robin(&self, upstream_id: uuid::Uuid) -> u64 {
        self.store.next_round_robin(upstream_id)
    }

    async fn chain(
        &self,
        ctx: &SecurityContext,
        tenant_id: uuid::Uuid,
    ) -> Result<Vec<uuid::Uuid>, OagwError> {
        tenant_chain(&self.tenant_resolver, ctx, tenant_id)
            .await
            .map_err(|err| OagwError::new(ErrorKind::ProtocolError, err.to_string()))
    }

    /// Resolve the effective configuration for a proxy request.
    ///
    /// # Errors
    ///
    /// Returns `route.not_found` when neither the alias nor a route matches,
    /// when a route matches the path but refuses the method, and
    /// `link.unavailable` when every candidate upstream is disabled.
    pub async fn resolve(
        &self,
        ctx: &SecurityContext,
        tenant_id: uuid::Uuid,
        alias: &str,
        method: &Method,
        path: &str,
    ) -> Result<EffectiveConfig, OagwError> {
        let normalized = normalize(alias);
        // The data plane is called anonymously by design: the alias carries the
        // tenant scope. When no upstream owns the alias, that scope is nil, and
        // a nil scope owns nothing and has no ancestors — so the miss is
        // reported before the tenant resolver is asked about a tenant that
        // does not exist (`DESIGN.md` §CRUD, "Inherited via tenant chain walk").
        if tenant_id.is_nil() {
            return Err(alias_miss(&normalized));
        }
        let chain = self.chain(ctx, tenant_id).await?;
        let mut candidates: Vec<Arc<Upstream>> = Vec::new();
        for tenant in &chain {
            if let Some(upstream) = self.store.upstream_by_alias(*tenant, &normalized) {
                candidates.push(upstream);
            }
        }
        if candidates.is_empty() {
            return Err(alias_miss(&normalized));
        }

        // An ancestor-disabled upstream cannot be re-enabled by a descendant.
        if candidates.iter().any(|upstream| !upstream.enabled) {
            let any = candidates.first().cloned();
            return Err(
                OagwError::new(ErrorKind::LinkUnavailable, "upstream is disabled")
                    .with_upstream(any.map_or(uuid::Uuid::nil(), |upstream| upstream.id)),
            );
        }

        let mut enforcing_tenants = Vec::new();
        let mut upstream_effective = None;
        let mut matched: Option<SelectedRoute> = None;
        // The path a route claims but its method allowlist refuses is a
        // rejection, not a fall-through (`DESIGN.md` §"Guard Rules"); the
        // closest upstream's refusal is the one reported unless a candidate
        // further up the chain matches the request outright.
        let mut refused: Option<String> = None;
        for (index, upstream) in candidates.iter().enumerate() {
            let routes = self.routes_for(upstream, index);
            if index == 0 {
                let (effective, tenants) = self.effective_upstream(&chain, upstream);
                enforcing_tenants = tenants;
                upstream_effective = Some(effective);
            }
            match matcher::select_route(&routes, method, path) {
                matcher::Selection::Matched(selected) => {
                    matched = Some(selected);
                    break;
                }
                matcher::Selection::MethodRejected(pattern) => {
                    if refused.is_none() {
                        refused = Some(pattern);
                    }
                }
                matcher::Selection::Unmatched => {}
            }
        }
        let upstream = upstream_effective.ok_or_else(|| {
            OagwError::new(
                ErrorKind::RouteNotFound,
                "no upstream is registered for this alias",
            )
        })?;
        let route = matched;
        if route.is_none()
            && let Some(pattern) = refused
        {
            return Err(OagwError::new(
                ErrorKind::RouteNotFound,
                format!("method {method} is not allowed for route path '{pattern}'"),
            ));
        }

        let mut rate_limits = Vec::new();
        if let Some(limit) = upstream.rate_limit {
            rate_limits.push((format!("upstream:{}", upstream.id), limit));
        }
        if let Some(route) = &route
            && let Some(limit) = route.route.rate_limit
        {
            rate_limits.push((format!("route:{}", route.route.id), limit));
        }

        let plugins = match &route {
            Some(selected) => merge_plugins(&upstream.plugins, &selected.route.plugins),
            None => upstream.plugins.items.clone(),
        };
        let bindings: Vec<Binding> = bindings_of(&plugins);

        // A route-level CORS replaces the upstream's for that route; a route
        // without one inherits the upstream's (`ADR 0004` "Upstream/Route CORS
        // Field": CORS is a first-class field on both).
        let cors = route
            .as_ref()
            .and_then(|selected| selected.route.cors.clone())
            .or_else(|| upstream.cors.clone());
        let headers = upstream.headers.clone();
        let auth = upstream.auth.clone();
        Ok(EffectiveConfig {
            upstream,
            route,
            rate_limits,
            cors,
            headers,
            plugins: bindings,
            auth,
            enforcing_tenants,
        })
    }

    /// Routes of `upstream`'s owner, filtered to the upstream itself.
    fn routes_for(&self, upstream: &Upstream, _index: usize) -> Vec<Arc<Route>> {
        self.store
            .routes_for_upstream(upstream.id)
            .into_iter()
            .collect()
    }

    /// Merge the ancestor sections of `chain` over the closest upstream.
    ///
    /// The chain is walked closest ancestor first, so a descendant's own value
    /// is always read before an inherited one and an `enforce`d ancestor value
    /// is never bypassed by shadowing (`DESIGN.md` §"Hierarchical
    /// Configuration"). Returns the merged upstream and the tenants whose rate
    /// limit contributed to the effective one.
    fn effective_upstream(
        &self,
        chain: &[uuid::Uuid],
        closest: &Upstream,
    ) -> (Arc<Upstream>, Vec<uuid::Uuid>) {
        let mut effective = closest.clone();
        let Some(index) = chain.iter().position(|tenant| tenant == &closest.tenant_id) else {
            return (Arc::new(effective), Vec::new());
        };
        let ancestors: Vec<Arc<Upstream>> = chain[index + 1..]
            .iter()
            .filter_map(|tenant| {
                self.store
                    .upstream_by_alias(*tenant, &crate::domain::alias::normalize(&closest.alias))
            })
            .collect();
        let mut contributors: Vec<uuid::Uuid> = Vec::new();

        // -- Rate limits: min(ancestor, descendant), stricter always wins ----
        let mut limit = closest
            .rate_limit
            .filter(|configured| configured.sharing != SharingMode::Inherit);
        if limit.is_some() {
            contributors.push(closest.tenant_id);
        }
        for ancestor in &ancestors {
            let Some(ancestor_limit) = ancestor.rate_limit else {
                continue;
            };
            match ancestor_limit.sharing {
                SharingMode::Enforce => {
                    limit = Some(tighten(limit, ancestor_limit));
                    if !contributors.contains(&ancestor.tenant_id) {
                        contributors.push(ancestor.tenant_id);
                    }
                }
                // An inherited limit is only adopted while nothing is known.
                SharingMode::Inherit if limit.is_none() => {
                    limit = Some(ancestor_limit);
                    if !contributors.contains(&ancestor.tenant_id) {
                        contributors.push(ancestor.tenant_id);
                    }
                }
                SharingMode::Inherit | SharingMode::Private => {}
            }
        }
        if contributors.len() > 1
            // Several limits contributed: what the request obeys is the
            // ancestor's enforced ceiling, not the descendant's choice.
            && let Some(merged) = &mut limit
        {
            merged.sharing = SharingMode::Enforce;
        }
        effective.rate_limit = limit;

        // -- CORS: union origins if `inherit`, forced if `enforce` ----------
        for ancestor in &ancestors {
            let Some(ancestor_cors) = &ancestor.cors else {
                continue;
            };
            if ancestor_cors.sharing == SharingMode::Enforce {
                effective.cors = Some(ancestor_cors.clone());
            } else if ancestor_cors.sharing == SharingMode::Inherit
                && let Some(child) = &effective.cors
            {
                let mut merged = child.clone();
                for origin in &ancestor_cors.allowed_origins {
                    if !merged.allowed_origins.contains(origin) {
                        merged.allowed_origins.push(origin.clone());
                    }
                }
                effective.cors = Some(merged);
            }
        }

        // -- Plugins: concatenate `ancestor.plugins + descendant.plugins` ----
        // Root first, so the merged chain runs the way the configuration reads:
        // the most distant ancestor's bindings execute before the closest
        // upstream's own.
        let mut items: Vec<PluginBindingDto> = Vec::new();
        for ancestor in ancestors.iter().rev() {
            items.extend(ancestor.plugins.items.iter().cloned());
        }
        items.extend(effective.plugins.items.iter().cloned());
        effective.plugins = PluginsConfig {
            sharing: effective.plugins.sharing,
            items,
        };

        // -- Auth: forced if `enforce`, override if `inherit` ---------------
        for ancestor in &ancestors {
            let Some(ancestor_auth) = &ancestor.auth else {
                continue;
            };
            match ancestor_auth.sharing {
                SharingMode::Enforce => effective.auth = Some(ancestor_auth.clone()),
                SharingMode::Inherit if effective.auth.is_none() => {
                    effective.auth = Some(ancestor_auth.clone());
                }
                SharingMode::Inherit | SharingMode::Private => {}
            }
        }

        for ancestor in &ancestors {
            for tag in &ancestor.tags {
                if !effective.tags.contains(tag) {
                    effective.tags.push(tag.clone());
                }
            }
        }
        (Arc::new(effective), contributors)
    }

    /// Execute the proxy request end to end.
    ///
    /// # Errors
    ///
    /// Returns a gateway error per `ADR 0007` for every failure mode, after the
    /// plugin chain's error hooks have been applied to it (`ADR 0002`).
    pub async fn proxy(&self, call: ProxyCall) -> Result<axum::response::Response, OagwError> {
        let security = Arc::clone(&call.security);
        let mut attributes = crate::domain::plugin::request_attributes(&call.headers);
        // Kept for the failure path, which runs after `call` was handed to the
        // relay.
        let alias = call.alias.clone();
        let origin = call.origin.clone();
        let method = call.method.clone();
        let mut plugins: Vec<Binding> = Vec::new();
        let mut cors_headers: Vec<(String, String)> = Vec::new();

        let outcome = match self
            .resolve(
                security.as_ref(),
                security.subject_tenant_id(),
                &alias,
                &method,
                &call.path,
            )
            .await
        {
            Ok(effective) => {
                plugins.clone_from(&effective.plugins);
                // CORS is validated before anything is dialed, and the headers
                // an allowed origin is answered with are kept for the failure
                // paths too: a rejected request is still a CORS response.
                cors_headers = validated_cors(
                    effective.cors.as_ref(),
                    call.origin.as_deref(),
                    &call.method,
                )?;
                self.relay(&effective, call, &cors_headers).await
            }
            // A resolve failure has no effective configuration, but the alias
            // may still name an upstream whose chain should report the failure.
            Err(error) => Err(error),
        };

        match outcome {
            Ok(response) => Ok(response),
            Err(error) => {
                if plugins.is_empty()
                    && let Some(upstream) = self
                        .closest_upstream(security.as_ref(), security.subject_tenant_id(), &alias)
                        .await
                {
                    plugins = bindings_of(&upstream.plugins.items);
                    if let Ok(headers) =
                        validated_cors(upstream.cors.as_ref(), origin.as_deref(), &method)
                    {
                        cors_headers = headers;
                    }
                }
                Err(self
                    .report(plugins, &mut attributes, &cors_headers, error)
                    .await)
            }
        }
    }

    /// Apply the chain's error hooks and the response headers a rejected
    /// request still carries, then hand the error back.
    async fn report(
        &self,
        plugins: Vec<Binding>,
        attributes: &mut BTreeMap<String, serde_json::Value>,
        cors_headers: &[(String, String)],
        mut error: OagwError,
    ) -> OagwError {
        transform_error(&self.transforms, &plugins, &mut error, attributes).await;
        // The correlation identifier reaches the wire even on a failure: the
        // error hook seeds it when the chain carries the request-id plugin.
        if let Some(id) = attributes
            .get(crate::infra::plugin::transform::REQUEST_ID_HEADER)
            .and_then(serde_json::Value::as_str)
        {
            error = error.with_header(crate::infra::plugin::transform::REQUEST_ID_HEADER, id);
        }
        for (name, value) in cors_headers {
            error = error.with_header(name, value);
        }
        error
    }

    /// The closest upstream owning `alias`, if the caller's scope can see one.
    ///
    /// Used to report a resolve failure: the alias names the configuration, so
    /// an upstream that exists but refuses the request still has a plugin chain
    /// whose error hooks and CORS policy apply to the rendered problem.
    async fn closest_upstream(
        &self,
        ctx: &SecurityContext,
        tenant_id: uuid::Uuid,
        alias: &str,
    ) -> Option<Arc<Upstream>> {
        if tenant_id.is_nil() {
            return None;
        }
        let Ok(chain) = self.chain(ctx, tenant_id).await else {
            return None;
        };
        let normalized = normalize(alias);
        chain
            .iter()
            .find_map(|tenant| self.store.upstream_by_alias(*tenant, &normalized))
    }

    /// Relay `call` against an already-resolved configuration.
    ///
    /// # Errors
    ///
    /// Returns a gateway error per `ADR 0007` for every failure mode.
    async fn relay(
        &self,
        effective: &EffectiveConfig,
        call: ProxyCall,
        cors_headers: &[(String, String)],
    ) -> Result<axum::response::Response, OagwError> {
        let security = Arc::clone(&call.security);
        let upstream = effective.upstream.clone();
        let route = effective.route.clone();
        // The normalized spelling is for matching only: what the upstream sees
        // is the path as the client spelled it, until a plugin rewrites it.
        let client_path = route.as_ref().map_or_else(
            || matcher::forward_path(&call.path),
            |selected| selected.upstream_path.clone(),
        );

        // -- Endpoint selection -------------------------------------------
        let round_robin = self.store.next_round_robin(upstream.id);
        let endpoint = select_endpoint(&upstream, call.target_host.as_deref(), round_robin)?;
        self.engine.check_endpoint(&endpoint).await?;

        // -- Plugin chain: request phase ------------------------------------
        let mut request_ctx = RequestContext {
            tenant_id: upstream.tenant_id,
            // The caller's own tenant, distinct from the resource owner above:
            // per-caller plugin state is keyed on it, so two tenants calling one
            // upstream never share credentials.
            caller_tenant_id: security.subject_tenant_id(),
            subject_id: security.subject_id().to_string(),
            method: call.method.clone(),
            path: client_path,
            query: call.query.clone(),
            headers: build_outbound_headers(&call.headers, effective.headers.as_ref()),
            inbound_headers: call.headers.clone(),
            client_ip: call.client_ip,
            security: Some(Arc::clone(&security)),
            config: serde_json::Value::Null,
            attributes: crate::domain::plugin::request_attributes(&call.headers),
        };

        // Auth runs before the rate limit (`ADR 0006`): a caller is identified
        // before its request is counted against a scoped bucket.
        self.authenticate(effective, &mut request_ctx, &upstream)
            .await?;

        // -- Rate limiting -------------------------------------------------
        let decisions = self.enforce_rate_limits(effective, &call, &upstream)?;
        self.guard_request(effective, &request_ctx).await?;
        self.transform_request(effective, &mut request_ctx).await?;

        // -- Outbound request ------------------------------------------------
        // Path and query are what the plugin chain left behind: an auth plugin
        // that injected a credential into the query, or a transform that
        // rewrote the path, is what the upstream must receive. The route's
        // allowlist still restricts what the *client* may send, so it is
        // applied here, after the plugins ran; parameters the plugins injected
        // are not client input and stay exempt from it.
        let client_keys = query_keys(call.query.as_deref());
        let injected: Vec<String> = query_keys(request_ctx.query.as_deref())
            .into_iter()
            .filter(|name| !client_keys.contains(name))
            .collect();
        let query = filter_query(request_ctx.query.as_deref(), route.as_ref(), &injected)?;
        let url = ProxyEngine::build_url(&endpoint, &request_ctx.path, query.as_deref())?;
        let mut headers = request_ctx.headers.clone();
        // The upstream's authority is what the origin reads: `host:port`, with
        // an IPv6 literal bracketed, exactly as the dial URL spells it.
        headers.insert(
            http::header::HOST,
            http::HeaderValue::from_str(&endpoint.authority()).map_err(|_| {
                OagwError::new(
                    ErrorKind::ProtocolError,
                    "upstream host is not a valid header",
                )
            })?,
        );

        if let Some(client_request) = call.upgrade_request {
            // An upgrade cannot be negotiated with the filtered header set: the
            // tokens the origin needs are exactly the ones the passthrough
            // filter drops, so they are restored from the client's request.
            restore_upgrade_headers(&mut headers, &call.headers);
            let outbound = OutboundRequest {
                method: call.method.clone(),
                url,
                headers,
                body: Bytes::new(),
            };
            return self
                .engine
                .relay_websocket(&endpoint, &outbound, client_request)
                .await;
        }

        let outbound = OutboundRequest {
            method: call.method.clone(),
            url,
            headers,
            body: call.body,
        };

        let response = self.engine.send(outbound).await?;
        let status = response.status();
        let content_type = response
            .headers()
            .get(http::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        let upstream_headers =
            self.upstream_response_headers(response.headers(), effective, cors_headers, &decisions);

        // -- Plugin chain: response phase ------------------------------------
        // Guards validate the relayed response before the caller sees it
        // (`ADR 0009`), and run before the transforms, as in the request phase.
        self.guard_response(
            effective,
            &request_ctx,
            &upstream,
            status,
            &upstream_headers,
        )
        .await?;
        let upstream_headers = self
            .transform_response(effective, &request_ctx, &upstream, status, upstream_headers)
            .await?;

        if is_streaming(content_type.as_deref()) {
            // Each frame is bounded by the same deadline as the exchange: the
            // client timeout only bounds the time to the response headers, so
            // an upstream that stops writing mid-body would otherwise pin the
            // request for good.
            let frames = crate::infra::proxy::bounded_frames(
                response.into_body(),
                self.config.proxy_timeout(),
            );
            return relayed_response(
                status,
                &upstream_headers,
                &upstream,
                route.as_ref(),
                axum::body::Body::from_stream(frames),
            );
        }

        let payload = response.bytes_within(self.config.proxy_timeout()).await?;
        relayed_response(
            status,
            &upstream_headers,
            &upstream,
            route.as_ref(),
            axum::body::Body::from(payload),
        )
    }

    /// Run the configured auth plugin, if the resolved chain has one.
    ///
    /// # Errors
    ///
    /// Returns `plugin.not_found` when the configured auth plugin has no backing
    /// implementation, and whatever the plugin itself reports.
    async fn authenticate(
        &self,
        effective: &EffectiveConfig,
        request_ctx: &mut RequestContext,
        upstream: &Upstream,
    ) -> Result<(), OagwError> {
        let Some(auth) = &effective.auth else {
            return Ok(());
        };
        let configured = auth.plugin_type.as_deref().unwrap_or_default();
        let Some(plugin) = self
            .auth_plugins
            .resolve(validation::plugin_registry_key(configured))
        else {
            return Err(OagwError::new(
                ErrorKind::PluginNotFound,
                "the configured auth plugin has no backing implementation",
            )
            .with_upstream(upstream.id));
        };
        request_ctx.config = auth.config.clone();
        plugin.authenticate(request_ctx).await?;
        request_ctx.config = serde_json::Value::Null;
        Ok(())
    }

    /// Charge the request against every configured rate limit.
    ///
    /// # Errors
    ///
    /// Returns `rate_limit.exceeded` for the first limit the request busts,
    /// carrying the `Retry-After` and the `X-RateLimit-*` context when the
    /// configuration asks for them.
    fn enforce_rate_limits(
        &self,
        effective: &EffectiveConfig,
        call: &ProxyCall,
        upstream: &Upstream,
    ) -> Result<EnforcedLimits, OagwError> {
        let mut decisions: EnforcedLimits = Vec::new();
        for (name, limit) in &effective.rate_limits {
            let identity = rate_identity(name, limit, effective, call);
            let decision = self.limiter.check(&identity, limit, limit.cost.max(1));
            if decision.allowed {
                decisions.push((name.clone(), *limit, decision));
                continue;
            }
            let mut error = OagwError::new(
                ErrorKind::RateLimitExceeded,
                format!("rate limit exceeded for upstream {}", upstream.alias),
            )
            .with_context("scope", serde_json::json!(name.to_owned()))
            .with_upstream(upstream.id);
            if limit.response_headers && self.config.rate_limit_response_headers {
                for (header, value) in rate_limit_headers(&decision) {
                    error = error.with_header(header, &value);
                }
            }
            return Err(error.with_retry_after_secs(decision.retry_after_secs));
        }
        Ok(decisions)
    }

    /// Let every configured guard validate the outbound request.
    ///
    /// # Errors
    ///
    /// Returns the guard's rejection as a problem response, and the guard's own
    /// error on an infrastructure failure.
    async fn guard_request(
        &self,
        effective: &EffectiveConfig,
        request_ctx: &RequestContext,
    ) -> Result<(), OagwError> {
        for binding in &effective.plugins {
            if let Some(guard) = self.guards.resolve(&binding.plugin_type) {
                let probe = clone_context(request_ctx, binding.config.clone());
                match guard.guard_request(&probe).await? {
                    GuardDecision::Allow => {}
                    GuardDecision::Reject(rejection) => return Err(rejected_request(rejection)),
                }
            }
        }
        Ok(())
    }

    /// Let every configured transform rewrite the outbound request.
    ///
    /// # Errors
    ///
    /// Returns whatever a transform reports.
    async fn transform_request(
        &self,
        effective: &EffectiveConfig,
        request_ctx: &mut RequestContext,
    ) -> Result<(), OagwError> {
        for binding in &effective.plugins {
            if let Some(transform) = self.transforms.resolve(&binding.plugin_type) {
                request_ctx.config = binding.config.clone();
                transform.transform_request(request_ctx).await?;
                request_ctx.config = serde_json::Value::Null;
            }
        }
        Ok(())
    }

    /// The upstream's response headers as the caller receives them: the
    /// upstream's own set, re-filtered, with the CORS and rate-limit headers the
    /// configuration asks for added.
    fn upstream_response_headers(
        &self,
        upstream_headers: &http::HeaderMap,
        effective: &EffectiveConfig,
        cors_headers: &[(String, String)],
        decisions: &EnforcedLimits,
    ) -> http::HeaderMap {
        let mut headers = upstream_headers.clone();
        apply_response_headers(&mut headers, effective.headers.as_ref());
        strip_hop_by_hop(&mut headers);
        for (name, value) in cors_headers {
            if let (Ok(name), Ok(value)) = (
                http::HeaderName::from_bytes(name.as_bytes()),
                http::HeaderValue::from_str(value),
            ) {
                headers.insert(name, value);
            }
        }
        mark_upstream_source(&mut headers);
        if self.config.rate_limit_response_headers
            && let Some((_, limit, decision)) = decisions.last()
            && limit.response_headers
        {
            for (header, value) in rate_limit_headers(decision) {
                if let (Ok(name), Ok(value)) = (
                    http::HeaderName::from_bytes(header.as_bytes()),
                    http::HeaderValue::from_str(&value),
                ) {
                    headers.insert(name, value);
                }
            }
        }
        headers
    }

    /// Let every configured guard validate the relayed response.
    ///
    /// # Errors
    ///
    /// Returns the guard's rejection as a problem response, and the guard's own
    /// error on an infrastructure failure.
    async fn guard_response(
        &self,
        effective: &EffectiveConfig,
        request_ctx: &RequestContext,
        upstream: &Upstream,
        status: StatusCode,
        headers: &http::HeaderMap,
    ) -> Result<(), OagwError> {
        for binding in &effective.plugins {
            if let Some(guard) = self.guards.resolve(&binding.plugin_type) {
                let response_ctx = ResponseContext {
                    tenant_id: upstream.tenant_id,
                    status,
                    headers: headers.clone(),
                    config: binding.config.clone(),
                    attributes: request_ctx.attributes.clone(),
                };
                match guard.guard_response(&response_ctx).await? {
                    GuardDecision::Allow => {}
                    GuardDecision::Reject(rejection) => return Err(rejected_request(rejection)),
                }
            }
        }
        Ok(())
    }

    /// Let every configured transform rewrite the relayed response.
    ///
    /// # Errors
    ///
    /// Returns whatever a transform reports.
    async fn transform_response(
        &self,
        effective: &EffectiveConfig,
        request_ctx: &RequestContext,
        upstream: &Upstream,
        status: StatusCode,
        mut headers: http::HeaderMap,
    ) -> Result<http::HeaderMap, OagwError> {
        for binding in &effective.plugins {
            if let Some(transform) = self.transforms.resolve(&binding.plugin_type) {
                let mut response_ctx = ResponseContext {
                    tenant_id: upstream.tenant_id,
                    status,
                    headers: headers.clone(),
                    config: binding.config.clone(),
                    attributes: request_ctx.attributes.clone(),
                };
                transform.transform_response(&mut response_ctx).await?;
                headers = response_ctx.headers;
            }
        }
        Ok(headers)
    }
}

/// Render the relayed upstream response, stamped with the configuration that
/// answered it so the transport layer can log the match.
///
/// # Errors
///
/// Returns a `protocol_error` when the status or the headers cannot form a
/// valid response.
fn relayed_response(
    status: StatusCode,
    upstream_headers: &http::HeaderMap,
    upstream: &Upstream,
    route: Option<&SelectedRoute>,
    body: axum::body::Body,
) -> Result<axum::response::Response, OagwError> {
    let mut builder = axum::response::Response::builder()
        .status(status)
        .extension(RelayedTarget {
            upstream_id: upstream.id,
            route_id: route.map(|selected| selected.route.id),
        });
    for (name, value) in upstream_headers {
        builder = builder.header(name, value);
    }
    builder
        .body(body)
        .map_err(|err| OagwError::new(ErrorKind::ProtocolError, err.to_string()))
}

/// `X-RateLimit-*` header values for a decision.
fn rate_limit_headers(decision: &RateDecision) -> [(&'static str, String); 3] {
    [
        ("x-ratelimit-limit", decision.limit.to_string()),
        ("x-ratelimit-remaining", decision.remaining.to_string()),
        ("x-ratelimit-reset", decision.reset_secs.to_string()),
    ]
}

/// The identity a `scope: user` limit is keyed on.
///
/// An identified caller keeps its own bucket; an anonymous one (nil subject id)
/// is keyed on the client address instead, so two distinct callers do not share
/// a single bucket, and finally on the literal `unknown` when no address is
/// available.
fn rate_user_identity(security: &Arc<SecurityContext>, client_ip: Option<IpAddr>) -> String {
    let subject = security.subject_id();
    if !subject.is_nil() {
        return subject.to_string();
    }
    client_ip.map_or_else(|| "unknown".to_owned(), |ip| ip.to_string())
}

fn clone_context(ctx: &RequestContext, config: serde_json::Value) -> RequestContext {
    let mut probe = ctx.clone();
    probe.config = config;
    probe
}

fn origin_allowed(config: &CorsConfig, origin: &str) -> bool {
    config
        .allowed_origins
        .iter()
        .any(|allowed| allowed == "*" || allowed == origin)
}

fn method_allowed(config: &CorsConfig, method: &Method) -> bool {
    config
        .allowed_methods
        .iter()
        .any(|allowed| Method::from_bytes(allowed.as_bytes()).is_ok_and(|parsed| parsed == method))
}

/// The response headers an allowed cross-origin request is answered with.
fn cors_headers_for(config: &CorsConfig, origin: &str) -> Vec<(String, String)> {
    let mut headers = vec![
        ("access-control-allow-origin".to_owned(), origin.to_owned()),
        ("vary".to_owned(), "Origin".to_owned()),
    ];
    if !config.expose_headers.is_empty() {
        headers.push((
            "access-control-expose-headers".to_owned(),
            config.expose_headers.join(", "),
        ));
    }
    if config.allow_credentials {
        headers.push((
            "access-control-allow-credentials".to_owned(),
            "true".to_owned(),
        ));
    }
    headers
}

/// Validate the CORS policy of an actual (non-preflight) request and build the
/// headers it is answered with (`ADR 0004`).
///
/// No `Origin` header, or a disabled configuration, means CORS does not apply
/// and no header is produced. A refused origin gets no `Access-Control-*`
/// header at all: only a validated origin may be echoed back.
///
/// # Errors
///
/// Returns `cors.origin_not_allowed` / `cors.method_not_allowed` on a refusal.
fn validated_cors(
    cors: Option<&CorsConfig>,
    origin: Option<&str>,
    method: &Method,
) -> Result<Vec<(String, String)>, OagwError> {
    let (Some(origin), Some(config)) = (origin, cors) else {
        return Ok(Vec::new());
    };
    if !config.enabled {
        return Ok(Vec::new());
    }
    if !origin_allowed(config, origin) {
        return Err(OagwError::new(
            ErrorKind::CorsOriginNotAllowed,
            format!("Origin '{origin}' not in allowed origins list"),
        ));
    }
    if !method_allowed(config, method) {
        return Err(OagwError::new(
            ErrorKind::CorsMethodNotAllowed,
            format!("Method '{}' not in allowed methods list", method.as_str()),
        ));
    }
    Ok(cors_headers_for(config, origin))
}

/// A guard rejection, rendered with the status the guard asked for.
///
/// The class stays a validation failure — the guard is a validation plugin —
/// while the phase decides the status: 400 in the request phase, 502 when the
/// upstream's own response is the offender (`ADR 0009`).
fn rejected_request(rejection: crate::domain::plugin::Rejection) -> OagwError {
    OagwError::new(ErrorKind::Validation, rejection.detail)
        .with_status(rejection.status)
        .with_context("error_code", serde_json::json!(rejection.error_code))
        .with_context("status", serde_json::json!(rejection.status.as_u16()))
}

/// The stricter of two limits: the lower sustained rate and the lower burst.
///
/// `current` carries the configuration the request is already known to obey;
/// `candidate` tightens it.
fn tighten(current: Option<RateLimitConfig>, candidate: RateLimitConfig) -> RateLimitConfig {
    let Some(base) = current else {
        return candidate;
    };
    let mut merged = base;
    if candidate.sustained.rate < merged.sustained.rate {
        merged.sustained.rate = candidate.sustained.rate;
    }
    merged.burst = Some(Burst {
        capacity: merged.capacity().min(candidate.capacity()),
    });
    merged
}

/// The executable form of a plugin binding list: canonical registry key plus
/// binding configuration.
#[must_use]
fn bindings_of(plugins: &[PluginBindingDto]) -> Vec<Binding> {
    plugins
        .iter()
        .map(|binding| match binding {
            PluginBindingDto::Ref(reference) => Binding {
                plugin_type: validation::plugin_registry_key(reference).to_owned(),
                config: serde_json::Value::Null,
            },
            PluginBindingDto::Detailed { plugin_ref, config } => Binding {
                plugin_type: validation::plugin_registry_key(plugin_ref).to_owned(),
                config: config.clone(),
            },
        })
        .collect()
}

/// Restrict the forwarded query to the route's allowlist.
///
/// Only a selected route can restrict a query: when nothing matched, the
/// client's own query travels upstream unchanged.
///
/// Filtering is done on the raw segments, so what survives is byte-identical to
/// what the client sent — re-serializing an escaped value would rewrite `a b`
/// into `a+b` and `/` into `%2F`. A parameter the client sent that the
/// allowlist does not name is a rejection (`DESIGN.md` §"Guard Rules": reject
/// if unknown), not a silent drop. Names in `injected` are parameters the
/// plugin chain added; they are not client input, so the allowlist does not
/// govern them.
///
/// # Errors
///
/// Returns a validation error when a client parameter is not allowlisted.
pub fn filter_query(
    query: Option<&str>,
    route: Option<&SelectedRoute>,
    injected: &[String],
) -> Result<Option<String>, OagwError> {
    let Some(query) = query.filter(|value| !value.is_empty()) else {
        return Ok(None);
    };
    let allowlist = route.and_then(|selected| {
        selected
            .route
            .match_rule
            .http()
            .map(|http| http.query_allowlist.clone())
    });
    let Some(allowlist) = allowlist else {
        return Ok(Some(query.to_owned()));
    };
    if allowlist.is_empty() {
        // An empty allowlist forwards nothing the client sent; parameters the
        // plugin chain injected are not the client's.
        return Ok(retain_only(query, injected));
    }
    let allowed: Vec<String> = allowlist
        .iter()
        .map(|name| name.trim().to_owned())
        .collect();
    let mut kept: Vec<&str> = Vec::new();
    for segment in query.split('&').filter(|segment| !segment.is_empty()) {
        let name = segment_key(segment);
        if allowed.contains(&name) || injected.contains(&name) {
            kept.push(segment);
        } else {
            return Err(OagwError::new(
                ErrorKind::Validation,
                format!("query parameter '{name}' is not in the route's query allowlist"),
            )
            .with_context("field", serde_json::json!("match.http.query_allowlist")));
        }
    }
    Ok((!kept.is_empty()).then(|| kept.join("&")))
}

/// The decoded parameter name of one raw `name=value` segment.
#[must_use]
fn segment_key(segment: &str) -> String {
    let raw = segment.split('=').next().unwrap_or(segment);
    form_urlencoded::parse(raw.as_bytes())
        .map(|(name, _)| name.into_owned())
        .next()
        .unwrap_or_default()
}

/// Every decoded parameter name of a raw query string.
#[must_use]
fn query_keys(query: Option<&str>) -> std::collections::BTreeSet<String> {
    query
        .map(|raw| {
            raw.split('&')
                .filter(|part| !part.is_empty())
                .map(segment_key)
                .collect()
        })
        .unwrap_or_default()
}

/// The segments of `query` whose parameter name is in `names`.
#[must_use]
fn retain_only(query: &str, names: &[String]) -> Option<String> {
    let kept: Vec<&str> = query
        .split('&')
        .filter(|segment| !segment.is_empty() && names.contains(&segment_key(segment)))
        .collect();
    (!kept.is_empty()).then(|| kept.join("&"))
}

/// Identity component of a rate-limit key.
fn rate_identity(
    name: &str,
    limit: &RateLimitConfig,
    effective: &EffectiveConfig,
    call: &ProxyCall,
) -> String {
    let identity = match limit.scope {
        crate::domain::model::RateScope::Global => "global".to_owned(),
        crate::domain::model::RateScope::Tenant => effective.upstream.tenant_id.to_string(),
        // A caller that reached the data plane anonymously has no subject: its
        // identity component is the client address, then the literal `unknown`.
        // Keying a nil subject would put every anonymous caller into one bucket
        // shared across the whole platform.
        crate::domain::model::RateScope::User => rate_user_identity(&call.security, call.client_ip),
        // The TCP peer is the best address the gateway has. Behind the platform
        // edge it is the edge's own address, so per-client buckets need the
        // edge to forward the real client address in its own connection
        // extension; a client-supplied `x-forwarded-for` is never trusted.
        crate::domain::model::RateScope::Ip => call
            .client_ip
            .map_or_else(|| "unknown".to_owned(), |ip| ip.to_string()),
        crate::domain::model::RateScope::Route => effective.route.as_ref().map_or_else(
            || "route".to_owned(),
            |selected| selected.route.id.to_string(),
        ),
    };
    scope_key(name, limit.scope, &identity)
}

/// Which configuration answered a relayed request, recorded on the response so
/// the transport layer can log the matched route without re-resolving the
/// alias.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RelayedTarget {
    /// Upstream the request was relayed to.
    pub upstream_id: uuid::Uuid,
    /// Route that matched, when one did.
    pub route_id: Option<uuid::Uuid>,
}

/// Everything the transport layer hands to the data plane.
pub struct ProxyCall {
    /// Upstream alias from the URL.
    pub alias: String,
    /// Path suffix after `/proxy/{alias}` (empty when absent).
    pub path: String,
    /// Raw query string from the client URL.
    pub query: Option<String>,
    /// Client method.
    pub method: Method,
    /// Client headers.
    pub headers: HeaderMap,
    /// Buffered client body.
    pub body: Bytes,
    /// `X-OAGW-Target-Host` when supplied.
    pub target_host: Option<String>,
    /// `Origin` header when supplied.
    pub origin: Option<String>,
    /// Authenticated caller identity.
    pub security: Arc<SecurityContext>,
    /// Best-effort client address.
    pub client_ip: Option<IpAddr>,
    /// The raw client request, present only for WebSocket upgrades.
    pub upgrade_request: Option<http::Request<axum::body::Body>>,
}

/// Whether a request is a CORS preflight (`ADR 0004`).
#[must_use]
pub fn is_preflight(method: &Method, headers: &HeaderMap) -> bool {
    method == Method::OPTIONS
        && headers.contains_key(http::header::ORIGIN)
        && headers.contains_key("access-control-request-method")
}

/// Build the permissive preflight response (`ADR 0004`).
#[must_use]
pub fn preflight_response(request_headers: &HeaderMap) -> axum::response::Response {
    let origin = request_headers
        .get(http::header::ORIGIN)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("*");
    let method = request_headers
        .get("access-control-request-method")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("*");
    let requested_headers = request_headers
        .get("access-control-request-headers")
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();

    let mut builder = axum::response::Response::builder()
        .status(StatusCode::NO_CONTENT)
        .header("access-control-allow-origin", origin)
        .header("access-control-allow-methods", method)
        .header(
            "access-control-max-age",
            crate::config::CORS_PREFLIGHT_MAX_AGE.to_string(),
        )
        .header(
            "vary",
            "Origin, Access-Control-Request-Method, Access-Control-Request-Headers",
        );
    if !requested_headers.is_empty() {
        builder = builder.header("access-control-allow-headers", requested_headers);
    }
    builder.body(axum::body::Body::empty()).unwrap_or_else(|_| {
        axum::response::Response::builder()
            .status(StatusCode::NO_CONTENT)
            .body(axum::body::Body::empty())
            .unwrap_or_default()
    })
}

/// Apply the plugin chain's error hooks to a gateway error.
pub async fn transform_error(
    transforms: &crate::domain::plugin::TransformPluginRegistry,
    plugins: &[Binding],
    error: &mut OagwError,
    attributes: &mut BTreeMap<String, serde_json::Value>,
) {
    for binding in plugins {
        if let Some(transform) = transforms.resolve(&binding.plugin_type) {
            let mut ctx = crate::domain::plugin::ErrorContext {
                error: error.clone(),
                config: binding.config.clone(),
                attributes: attributes.clone(),
            };
            if transform.transform_error(&mut ctx).await.is_ok() {
                *error = ctx.error;
                // Hook-mutated attributes (a minted `x-request-id`, for
                // instance) have to survive the hook: the caller renders them
                // onto the problem response.
                *attributes = ctx.attributes;
            }
        }
    }
}

/// Deadline helper for transports.
#[must_use]
pub fn deadline(config: &OagwConfig) -> Duration {
    config.proxy_timeout()
}
