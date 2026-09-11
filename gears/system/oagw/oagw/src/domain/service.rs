//! The control-plane service: CRUD, validation, alias resolution and the
//! hierarchical configuration merge.
//!
//! This is the seam between the wire layer (`crate::api`) and the domain: the
//! handlers hand it a [`SecurityContext`] and a draft, and get back either a
//! persisted resource or a [`DomainError`] carrying the problem type the
//! specification assigns to that failure.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::config::OagwConfig;
use crate::credstore_client::SharedCredentialStore;
use crate::domain::model::{
    CorsRule, Endpoint, HttpMatch, MatchRule, PathSuffixMode, PluginBinding, PluginDefinition,
    PluginType, RateLimitRule, Route, Scheme, Sharing, Upstream,
};
use crate::domain::plugin::{ControlPlane, GuardPlugin, TransformPlugin};
use crate::domain::query::OutboundQuery;
use crate::domain::ratelimit::SharedRateLimiter;
use crate::domain::store::SharedStore;
use crate::error::{DomainError, ErrorKind};
use crate::ids;
use serde_json::Value;

/// A resource id as it appears on the wire: `<gts type>~<uuid>`.
#[must_use]
pub fn instance_id(resource_type: &str, id: Uuid) -> String {
    format!("{resource_type}{id}")
}

/// The id a wire reference names, or `None` when it is malformed.
#[must_use]
pub fn parse_instance_id(reference: &str) -> Option<Uuid> {
    let instance = reference.rsplit('~').next()?;
    Uuid::parse_str(instance).ok()
}

/// The result of resolving a proxy request against the control plane.
#[derive(Debug, Clone)]
pub struct ResolvedRequest {
    /// The upstream the request resolves to — the closest match in the chain.
    pub upstream: Upstream,
    /// The matched route. Proxy requests without one are refused.
    pub route: Route,
    /// The endpoint the request is sent to.
    pub endpoint: Endpoint,
    /// The tenant chain the alias was resolved over, descendant first.
    pub chain: Vec<Uuid>,
    /// Configuration after the hierarchy merge.
    pub effective: EffectiveConfig,
    /// The path of the outbound hop, after the route's suffix mode is applied.
    pub outbound_path: String,
}

/// Configuration after the hierarchy merge.
#[derive(Debug, Clone, Default)]
pub struct EffectiveConfig {
    /// Auth configuration actually in force.
    pub auth: Option<crate::domain::model::AuthConfig>,
    /// Request/response header rules of the selected upstream.
    pub headers: crate::domain::model::HeaderRules,
    /// The plugin chain: enforced ancestors, then the upstream's, then the route's.
    pub plugins: Vec<PluginBinding>,
    /// The strictest applicable rate limit.
    pub rate_limit: Option<RateLimitRule>,
    /// The CORS policy in force.
    pub cors: Option<CorsRule>,
    /// The union of ancestor, upstream and route tags.
    pub tags: Vec<String>,
}

/// Everything the control and data planes share.
pub struct Service {
    /// Configuration store.
    pub store: SharedStore,
    /// Plugin registry — the built-ins, resolved in process.
    pub plugins: Arc<ControlPlane>,
    /// Credential store client.
    pub credential_store: SharedCredentialStore,
    /// Tenant hierarchy resolver, when the platform provides one.
    pub tenants: Option<Arc<dyn tenant_resolver_sdk::TenantResolverClient>>,
    /// Gear configuration.
    pub config: OagwConfig,
    /// Rate-limit buckets.
    pub rate_limiter: SharedRateLimiter,
    round_robin: AtomicU64,
}

impl std::fmt::Debug for Service {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Service")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

impl Service {
    /// Build a service over `store`, `plugins` and `credential_store`.
    #[must_use]
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        store: SharedStore,
        plugins: Arc<ControlPlane>,
        credential_store: SharedCredentialStore,
        tenants: Option<Arc<dyn tenant_resolver_sdk::TenantResolverClient>>,
        config: OagwConfig,
        rate_limiter: SharedRateLimiter,
    ) -> Arc<Self> {
        Arc::new(Self {
            store,
            plugins,
            credential_store,
            tenants,
            config,
            rate_limiter,
            round_robin: AtomicU64::new(0),
        })
    }

    /// The ancestor chain of the caller's tenant, descendant first.
    ///
    /// Falls back to a single-tenant chain when no resolver is configured or
    /// the lookup fails: the data plane keeps working, it just treats the
    /// caller as its own root.
    pub async fn tenant_chain(&self, security: &SecurityContext) -> Vec<Uuid> {
        let tenant_id = security.subject_tenant_id();
        let Some(resolver) = &self.tenants else {
            return vec![tenant_id];
        };
        let options = tenant_resolver_sdk::GetAncestorsOptions::default();
        match resolver
            .get_ancestors(security, tenant_resolver_sdk::TenantId(tenant_id), &options)
            .await
        {
            Ok(response) => {
                let mut chain = Vec::with_capacity(response.ancestors.len() + 1);
                chain.push(response.tenant.id.0);
                chain.extend(response.ancestors.iter().map(|tenant| tenant.id.0));
                chain
            }
            Err(err) => {
                tracing::debug!(
                    error = %err,
                    "tenant ancestor lookup failed; treating the caller as its own root"
                );
                vec![tenant_id]
            }
        }
    }

    // -- upstream management ------------------------------------------------

    /// Create an upstream.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::Validation`] when the configuration is invalid,
    /// [`ErrorKind::Conflict`] when the alias is already taken in the tenant.
    pub async fn create_upstream(
        &self,
        security: &SecurityContext,
        mut draft: Upstream,
    ) -> Result<Upstream, DomainError> {
        draft.id = Uuid::new_v4();
        draft.tenant_id = security.subject_tenant_id();
        draft.created_at = Some(crate::domain::clock::now_rfc3339());
        draft.updated_at = None;
        self.validate_upstream(&draft)?;
        draft.alias = derive_or_require(&draft)?;
        if self.store.alias_taken(&draft.tenant_id, &draft.alias) {
            return Err(conflict(format!(
                "an upstream with alias {:?} already exists in this tenant",
                draft.alias
            )));
        }
        self.store.insert_upstream(draft.clone());
        Ok(draft)
    }

    /// List the calling tenant's upstreams, shaped for the wire.
    #[must_use]
    pub fn list_upstreams(
        &self,
        tenant_id: &Uuid,
        query: &crate::domain::list::ListQuery,
    ) -> Vec<serde_json::Value> {
        let rows = self
            .store
            .list_upstreams(tenant_id)
            .iter()
            .map(|upstream| serde_json::to_value(upstream).unwrap_or(serde_json::Value::Null))
            .collect();
        query.apply(rows)
    }

    /// Fetch one upstream.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::RouteNotFound`] when the id does not belong to the tenant.
    pub fn get_upstream(&self, tenant_id: &Uuid, id: &Uuid) -> Result<Upstream, DomainError> {
        self.store
            .get_upstream(id)
            .filter(|upstream| &upstream.tenant_id == tenant_id)
            .ok_or_else(|| not_found("upstream", id))
    }

    /// Replace an upstream.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::RouteNotFound`] when the id is unknown to the tenant,
    /// [`ErrorKind::Validation`] when the replacement is invalid or would move
    /// the alias.
    pub async fn replace_upstream(
        &self,
        security: &SecurityContext,
        id: &Uuid,
        mut replacement: Upstream,
    ) -> Result<Upstream, DomainError> {
        let tenant_id = security.subject_tenant_id();
        let existing = self.get_upstream(&tenant_id, id)?;
        replacement.id = existing.id;
        replacement.tenant_id = existing.tenant_id;
        replacement.created_at = existing.created_at.clone();
        replacement.updated_at = Some(crate::domain::clock::now_rfc3339());
        self.validate_upstream(&replacement)?;
        enforce_alias_update(&existing, &replacement)?;
        self.store
            .update_upstream(replacement.clone())
            .map_err(|err| DomainError::new(ErrorKind::RouteNotFound, err))?;
        Ok(replacement)
    }

    /// Delete an upstream.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::RouteNotFound`] when the id is unknown to the tenant,
    /// [`ErrorKind::PluginInUse`] when routes still reference it.
    pub fn delete_upstream(&self, tenant_id: &Uuid, id: &Uuid) -> Result<Upstream, DomainError> {
        let existing = self.get_upstream(tenant_id, id)?;
        let dependents = self.store.routes_for_upstream(tenant_id, id);
        if !dependents.is_empty() {
            return Err(DomainError::new(
                ErrorKind::PluginInUse,
                format!(
                    "upstream {} is referenced by {} route(s); delete them first",
                    ids::UPSTREAM_RESOURCE_TYPE,
                    dependents.len()
                ),
            ));
        }
        self.store
            .delete_upstream(id)
            .ok_or_else(|| not_found("upstream", id))?;
        Ok(existing)
    }

    // -- route management ---------------------------------------------------

    /// Create a route.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::Validation`] when the match rule or the upstream reference
    /// is invalid; [`ErrorKind::Conflict`] on a duplicate match rule.
    pub fn create_route(
        &self,
        security: &SecurityContext,
        mut draft: Route,
    ) -> Result<Route, DomainError> {
        draft.id = Uuid::new_v4();
        draft.tenant_id = security.subject_tenant_id();
        draft.created_at = Some(crate::domain::clock::now_rfc3339());
        draft.updated_at = None;
        self.validate_route(&draft, &draft.tenant_id)?;
        if self.route_duplicated(&draft) {
            return Err(conflict(
                "a route with this method and path already exists for the upstream",
            ));
        }
        self.store.insert_route(draft.clone());
        Ok(draft)
    }

    /// List the calling tenant's routes, shaped for the wire.
    #[must_use]
    pub fn list_routes(
        &self,
        tenant_id: &Uuid,
        query: &crate::domain::list::ListQuery,
    ) -> Vec<serde_json::Value> {
        let rows = self
            .store
            .list_routes(tenant_id)
            .iter()
            .map(|route| serde_json::to_value(route).unwrap_or(serde_json::Value::Null))
            .collect();
        query.apply(rows)
    }

    /// Fetch one route.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::RouteNotFound`] when the id does not belong to the tenant.
    pub fn get_route(&self, tenant_id: &Uuid, id: &Uuid) -> Result<Route, DomainError> {
        self.store
            .get_route(id)
            .filter(|route| &route.tenant_id == tenant_id)
            .ok_or_else(|| not_found("route", id))
    }

    /// Replace a route. `upstream_id` is immutable and taken from the stored
    /// definition.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::RouteNotFound`] when the id is unknown to the tenant,
    /// [`ErrorKind::Validation`] when the replacement is invalid.
    pub fn replace_route(
        &self,
        tenant_id: &Uuid,
        id: &Uuid,
        mut replacement: Route,
    ) -> Result<Route, DomainError> {
        let existing = self.get_route(tenant_id, id)?;
        replacement.id = existing.id;
        replacement.tenant_id = existing.tenant_id;
        replacement.upstream_id = existing.upstream_id;
        replacement.created_at = existing.created_at.clone();
        replacement.updated_at = Some(crate::domain::clock::now_rfc3339());
        self.validate_route(&replacement, tenant_id)?;
        if self.route_duplicated(&replacement) {
            return Err(conflict(
                "a route with this method and path already exists for the upstream",
            ));
        }
        self.store
            .update_route(replacement.clone())
            .map_err(|err| DomainError::new(ErrorKind::RouteNotFound, err))?;
        Ok(replacement)
    }

    /// Delete a route.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::RouteNotFound`] when the id is unknown to the tenant.
    pub fn delete_route(&self, tenant_id: &Uuid, id: &Uuid) -> Result<Route, DomainError> {
        let existing = self.get_route(tenant_id, id)?;
        self.store
            .delete_route(id)
            .ok_or_else(|| not_found("route", id))?;
        Ok(existing)
    }

    // -- plugin management --------------------------------------------------

    /// Create a custom plugin definition. Plugins are immutable: there is no
    /// replace.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::Validation`] when the definition is incomplete.
    pub fn create_plugin(
        &self,
        tenant_id: &Uuid,
        mut draft: PluginDefinition,
    ) -> Result<PluginDefinition, DomainError> {
        if draft.name.trim().is_empty() {
            return Err(validation("a plugin definition requires a `name`"));
        }
        if draft.config.is_null() {
            return Err(validation(
                "a plugin definition requires a `config` payload",
            ));
        }
        validate_plugin_config(&draft)?;
        let id = Uuid::new_v4();
        draft.id = id;
        draft.tenant_id = *tenant_id;
        draft.plugin_ref = instance_id(draft.plugin_type.resource_type(), id);
        draft.version = 1;
        draft.created_at = Some(crate::domain::clock::now_rfc3339());
        self.store.insert_plugin(draft.clone());
        Ok(draft)
    }

    /// List the calling tenant's plugin definitions, shaped for the wire.
    #[must_use]
    pub fn list_plugins(
        &self,
        tenant_id: &Uuid,
        query: &crate::domain::list::ListQuery,
    ) -> Vec<serde_json::Value> {
        let rows = self
            .store
            .list_plugins(tenant_id)
            .iter()
            .map(|plugin| serde_json::to_value(plugin).unwrap_or(serde_json::Value::Null))
            .collect();
        query.apply(rows)
    }

    /// Fetch one plugin definition.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::RouteNotFound`] when the id does not belong to the tenant.
    pub fn get_plugin(&self, tenant_id: &Uuid, id: &Uuid) -> Result<PluginDefinition, DomainError> {
        self.store
            .get_plugin(id)
            .filter(|plugin| &plugin.tenant_id == tenant_id)
            .ok_or_else(|| not_found("plugin", id))
    }

    /// Delete a plugin definition.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::PluginInUse`] when an upstream or a route still binds it.
    pub fn delete_plugin(
        &self,
        tenant_id: &Uuid,
        id: &Uuid,
    ) -> Result<PluginDefinition, DomainError> {
        let existing = self.get_plugin(tenant_id, id)?;
        let upstreams = self.store.upstreams_referencing(&existing.plugin_ref);
        let routes = self.store.routes_referencing(&existing.plugin_ref);
        if !upstreams.is_empty() || !routes.is_empty() {
            return Err(DomainError::new(
                ErrorKind::PluginInUse,
                format!(
                    "plugin {} is referenced by {} upstream(s) and {} route(s)",
                    existing.plugin_ref,
                    upstreams.len(),
                    routes.len()
                ),
            )
            .with_extension("plugin_id", existing.plugin_ref.clone())
            .with_extension(
                "referenced_by",
                serde_json::json!({
                    "upstreams": upstreams
                        .iter()
                        .map(|upstream| instance_id(ids::UPSTREAM_RESOURCE_TYPE, upstream.id))
                        .collect::<Vec<_>>(),
                    "routes": routes
                        .iter()
                        .map(|route| instance_id(ids::ROUTE_RESOURCE_TYPE, route.id))
                        .collect::<Vec<_>>(),
                }),
            ));
        }
        self.store
            .delete_plugin(id)
            .ok_or_else(|| not_found("plugin", id))?;
        Ok(existing)
    }

    /// The source payload of a custom plugin, as `GET …/source` returns it.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::RouteNotFound`] when the plugin is unknown to the tenant.
    pub fn plugin_source(
        &self,
        tenant_id: &Uuid,
        id: &Uuid,
    ) -> Result<serde_json::Value, DomainError> {
        let plugin = self.get_plugin(tenant_id, id)?;
        let source = plugin
            .source_code
            .clone()
            .or_else(|| match &plugin.config {
                serde_json::Value::String(text) => Some(text.clone()),
                serde_json::Value::Object(fields) => fields
                    .get("source")
                    .or_else(|| fields.get("source_code"))
                    .or_else(|| fields.get("code"))
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                _ => None,
            })
            .map(serde_json::Value::String)
            .unwrap_or(serde_json::Value::Null);
        Ok(serde_json::json!({
            "id": plugin.plugin_ref,
            "type": plugin.plugin_type,
            "name": plugin.name,
            "version": plugin.version,
            "source": source,
        }))
    }

    /// Resolve a guard binding to an executable plugin.
    ///
    /// Named identifiers come from the in-process registry; UUID-backed ones
    /// from the calling tenant's definitions (DESIGN §3.1 "Resolution
    /// Algorithm"), which also requires the definition to be of the slot's
    /// type. `Ok(None)` is a binding that lives in the *other* slot — the chain
    /// skips it there — while a ref that resolves to nothing at all is
    /// [`ErrorKind::PluginNotFound`].
    ///
    /// # Errors
    ///
    /// [`ErrorKind::PluginNotFound`] when nothing answers to `plugin_ref`.
    pub fn guard_plugin(
        &self,
        binding: &PluginBinding,
    ) -> Result<Option<std::sync::Arc<dyn GuardPlugin>>, DomainError> {
        if let Some(plugin) = self.plugins.guard_plugin(&binding.plugin_ref) {
            return Ok(Some(plugin));
        }
        match self.definition_for(binding)? {
            Some(definition) if definition.plugin_type == PluginType::Guard => Ok(Some(
                std::sync::Arc::new(crate::infra::plugins::DefinitionPlugin::guard(&definition)),
            )),
            Some(_) => Ok(None),
            None => Ok(None),
        }
    }

    /// Resolve a transform binding to an executable plugin; see [`Self::guard_plugin`].
    ///
    /// # Errors
    ///
    /// [`ErrorKind::PluginNotFound`] when nothing answers to `plugin_ref`.
    pub fn transform_plugin(
        &self,
        binding: &PluginBinding,
    ) -> Result<Option<std::sync::Arc<dyn TransformPlugin>>, DomainError> {
        if let Some(plugin) = self.plugins.transform_plugin(&binding.plugin_ref) {
            return Ok(Some(plugin));
        }
        match self.definition_for(binding)? {
            Some(definition) if definition.plugin_type == PluginType::Transform => Ok(Some(
                std::sync::Arc::new(
                    crate::infra::plugins::DefinitionPlugin::transform(&definition),
                ),
            )),
            Some(_) => Ok(None),
            None => Ok(None),
        }
    }

    /// The stored definition a UUID-backed binding names, or `None` when the
    /// ref resolves to a named plugin of another slot.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::PluginNotFound`] when the ref is UUID-backed and names no
    /// stored definition, or names one under a different type prefix.
    fn definition_for(&self, binding: &PluginBinding) -> Result<Option<PluginDefinition>, DomainError> {
        let Some(id) = parse_instance_id(&binding.plugin_ref) else {
            return Ok(None);
        };
        let Some(definition) = self.store.get_plugin(&id) else {
            return Err(DomainError::new(
                ErrorKind::PluginNotFound,
                format!(
                    "plugin {:?} is not a known plugin definition",
                    binding.plugin_ref
                ),
            ));
        };
        if definition.plugin_ref != binding.plugin_ref {
            return Err(DomainError::new(
                ErrorKind::PluginNotFound,
                format!(
                    "plugin {:?} does not name the definition registered as {:?}",
                    binding.plugin_ref, definition.plugin_ref
                ),
            ));
        }
        Ok(Some(definition))
    }

    // -- data plane ---------------------------------------------------------

    /// Resolve a proxy request: upstream by alias over the tenant chain, then
    /// a route, then an endpoint.
    ///
    /// # Errors
    ///
    /// The proxy error taxonomy: missing / invalid / unknown target host,
    /// route not found, disabled upstream, plaintext refused.
    // One input per stage of the resolution order the data plane already
    // walked; a struct would only rename them.
    #[allow(clippy::too_many_arguments)]
    pub async fn resolve_proxy(
        &self,
        security: &SecurityContext,
        alias_name: &str,
        path_suffix: Option<&str>,
        method: &http::Method,
        raw_query: Option<&str>,
        // Rate limiting keys an `ip`-scoped bucket on this; resolution itself
        // does not need it, and `check_rate_limit` takes it separately.
        _client_ip: &str,
        target_host: Option<&str>,
    ) -> Result<ResolvedRequest, DomainError> {
        let chain = self.tenant_chain(security).await;
        let normalized = crate::domain::alias::normalize(alias_name);
        let candidates = self.upstreams_for_alias(&chain, &normalized);
        let Some(upstream) = candidates.into_iter().next() else {
            return Err(DomainError::new(
                ErrorKind::RouteNotFound,
                format!("no upstream is registered for alias {normalized:?}"),
            )
            .with_alias(&normalized));
        };
        if !upstream.enabled {
            return Err(DomainError::new(
                ErrorKind::LinkUnavailable,
                format!("upstream {normalized} is disabled"),
            )
            .with_alias(&normalized)
            .with_upstream_id(upstream.id));
        }
        if !self.config.allow_http_upstream && !endpoint_schemes_are_tls(&upstream) {
            return Err(DomainError::new(
                ErrorKind::LinkUnavailable,
                format!(
                    "upstream {normalized} declares plaintext endpoints and \
                     allow_http_upstream is disabled"
                ),
            )
            .with_alias(&normalized)
            .with_upstream_id(upstream.id));
        }

        // Shadowing upstreams of the same alias: their routes are reachable
        // through the same proxy URL, so they are candidates for matching.
        let mut candidate_ids = vec![upstream.id];
        for shadowed in self.upstreams_for_alias(&chain, &normalized) {
            if shadowed.id != upstream.id {
                candidate_ids.push(shadowed.id);
            }
        }

        let route = self
            .matching_route(&chain, &candidate_ids, method, path_suffix)
            .ok_or_else(|| {
                DomainError::new(
                    ErrorKind::RouteNotFound,
                    format!("no route of upstream {normalized} matches {method} {path_suffix:?}"),
                )
                .with_alias(&normalized)
                .with_upstream_id(upstream.id)
            })?;

        self.validate_query(&route, raw_query)?;
        let endpoint = self.select_endpoint(&upstream, &normalized, target_host)?;
        let effective = self.effective_config(&chain, &upstream, Some(&route));
        let outbound_path = outbound_path(&route, path_suffix);

        Ok(ResolvedRequest {
            upstream,
            route,
            endpoint,
            chain,
            effective,
            outbound_path,
        })
    }

    /// Every upstream in the chain registered under `normalized_alias`,
    /// closest tenant first, including disabled ones — the caller decides what
    /// to do with those.
    #[must_use]
    pub fn upstreams_for_alias(&self, chain: &[Uuid], normalized_alias: &str) -> Vec<Upstream> {
        let all = self.store.all_upstreams();
        let mut found: Vec<Upstream> = Vec::new();
        for tenant_id in chain {
            for upstream in &all {
                if &upstream.tenant_id == tenant_id
                    && upstream.alias.eq_ignore_ascii_case(normalized_alias)
                    && !found.iter().any(|existing| existing.id == upstream.id)
                {
                    found.push(upstream.clone());
                }
            }
        }
        found
    }

    /// Pick the endpoint a request is sent to (ADR 0001's behaviour matrix).
    ///
    /// # Errors
    ///
    /// [`ErrorKind::MissingTargetHost`] when a common-suffix pool needs
    /// disambiguating, [`ErrorKind::InvalidTargetHost`] when the supplied
    /// header is not a bare hostname or IP, [`ErrorKind::UnknownTargetHost`]
    /// when it names no configured endpoint.
    fn select_endpoint(
        &self,
        upstream: &Upstream,
        normalized_alias: &str,
        target_host: Option<&str>,
    ) -> Result<Endpoint, DomainError> {
        let endpoints = &upstream.server.endpoints;
        if endpoints.is_empty() {
            return Err(DomainError::new(
                ErrorKind::Validation,
                format!("upstream {normalized_alias} has no endpoints"),
            )
            .with_alias(normalized_alias)
            .with_upstream_id(upstream.id));
        }

        // The header is validated whenever it is present, including for a
        // single-endpoint upstream where it would have been optional.
        if let Some(host) = target_host.map(crate::domain::alias::normalize) {
            if let Err(reason) = validate_target_host(&host) {
                return Err(DomainError::new(
                    ErrorKind::InvalidTargetHost,
                    format!("X-OAGW-Target-Host {host:?} is not a bare hostname or IP: {reason}"),
                )
                .with_alias(normalized_alias)
                .with_extension("valid_hosts", valid_hosts(endpoints)));
            }
            let Some(endpoint) = endpoints
                .iter()
                .find(|endpoint| crate::domain::alias::normalize(&endpoint.host) == host)
                .cloned()
            else {
                return Err(DomainError::new(
                    ErrorKind::UnknownTargetHost,
                    format!("X-OAGW-Target-Host {host:?} does not match any configured endpoint"),
                )
                .with_alias(normalized_alias)
                .with_host(&host)
                .with_extension("valid_hosts", valid_hosts(endpoints)));
            };
            return Ok(endpoint);
        }

        if crate::domain::alias::needs_target_host(endpoints, normalized_alias) {
            return Err(DomainError::new(
                ErrorKind::MissingTargetHost,
                format!(
                    "X-OAGW-Target-Host header required for multi-endpoint upstream with \
                     common suffix alias. Valid hosts: [{}]",
                    hosts_csv(endpoints)
                ),
            )
            .with_alias(normalized_alias)
            .with_extension("valid_hosts", valid_hosts(endpoints)));
        }

        let index = self.round_robin.fetch_add(1, Ordering::Relaxed) as usize % endpoints.len();
        Ok(endpoints[index].clone())
    }

    /// The first route in the chain that matches, closest tenant first; within
    /// a tenant the longest path prefix wins.
    fn matching_route(
        &self,
        chain: &[Uuid],
        candidate_upstream_ids: &[Uuid],
        method: &http::Method,
        path_suffix: Option<&str>,
    ) -> Option<Route> {
        for tenant_id in chain {
            let mut best: Option<(usize, Route)> = None;
            for route in self.store.list_routes(tenant_id) {
                if !route.enabled || !candidate_upstream_ids.contains(&route.upstream_id) {
                    continue;
                }
                let MatchRule::Http(http) = &route.match_rule else {
                    continue;
                };
                if !crate::domain::model::method_matches(method, &http.methods) {
                    continue;
                }
                if !prefix_matches(http, path_suffix) {
                    continue;
                }
                if best
                    .as_ref()
                    .is_none_or(|(best_len, _)| http.path.len() > *best_len)
                {
                    best = Some((http.path.len(), route));
                }
            }
            if let Some((_, route)) = best {
                return Some(route);
            }
        }
        None
    }

    /// Merge configuration over the tenant chain.
    ///
    /// The selected upstream is the base and a matched route overrides it.
    /// An ancestor upstream registered under the same alias contributes its
    /// `enforce`d constraints, which shadowing cannot relax, and its tags,
    /// which are additive.
    #[must_use]
    pub fn effective_config(
        &self,
        chain: &[Uuid],
        selected: &Upstream,
        route: Option<&Route>,
    ) -> EffectiveConfig {
        let ancestors: Vec<Upstream> = self
            .upstreams_for_alias(chain, &selected.alias)
            .into_iter()
            .filter(|upstream| upstream.id != selected.id)
            .collect();

        // Auth: the selected upstream's configuration stands unless an
        // ancestor marked its own `enforce` — that one cannot be overridden.
        // With no local auth, the nearest `inherit`ed one applies.
        let mut auth = selected.auth.clone();
        for ancestor in ancestors.iter().rev() {
            match ancestor.auth.as_ref() {
                Some(enforced) if enforced.sharing == Sharing::Enforce => {
                    auth = Some(enforced.clone());
                    break;
                }
                Some(_) if auth.is_none() => auth = ancestor.auth.clone(),
                _ => {}
            }
        }

        // Rate limits: the strictest of the selected upstream, the route, and
        // every enforced ancestor limit.
        let mut rate_limit = selected.rate_limit.clone();
        for ancestor in &ancestors {
            if let Some(limit) = ancestor
                .rate_limit
                .as_ref()
                .filter(|limit| limit.sharing == Sharing::Enforce)
            {
                rate_limit = Some(match rate_limit.take() {
                    Some(existing) => existing.stricter_of(limit),
                    None => limit.clone(),
                });
            }
        }
        if let Some(limit) = route.and_then(|route| route.rate_limit.as_ref()) {
            rate_limit = Some(match rate_limit.take() {
                Some(existing) => existing.stricter_of(limit),
                None => limit.clone(),
            });
        }

        // Plugins: enforced ancestor plugins first, then the upstream's, then
        // the route's — they run in that order (ADR 0002).
        let mut plugins = Vec::new();
        for ancestor in ancestors.iter().rev() {
            if ancestor.plugins.sharing == Sharing::Enforce {
                plugins.extend(ancestor.plugins.items.iter().cloned());
            }
        }
        plugins.extend(selected.plugins.items.iter().cloned());
        if let Some(route) = route {
            plugins.extend(route.plugins.items.iter().cloned());
        }

        // Tags are additive across the whole hierarchy.
        let mut tags = Vec::new();
        let tag_sources = ancestors
            .iter()
            .rev()
            .map(|upstream| &upstream.tags)
            .chain(std::iter::once(&selected.tags));
        for source in tag_sources {
            for tag in source {
                if !tags.contains(tag) {
                    tags.push(tag.clone());
                }
            }
        }
        if let Some(route) = route {
            for tag in &route.tags {
                if !tags.contains(tag) {
                    tags.push(tag.clone());
                }
            }
        }

        EffectiveConfig {
            auth,
            headers: selected.headers.clone(),
            plugins,
            rate_limit,
            cors: selected.cors.clone(),
            tags,
        }
    }

    /// Evaluate the effective rate limit for one request.
    ///
    /// A `queue` or `degrade` strategy degrades to admitting the request:
    /// neither is implemented in this release.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::RateLimit`] with a `Retry-After` when the bucket refuses.
    pub fn check_rate_limit(
        &self,
        resolved: &ResolvedRequest,
        tenant_id: Uuid,
        subject_id: Uuid,
        client_ip: &str,
        now_ms: u64,
    ) -> Result<(), DomainError> {
        let Some(rule) = resolved.effective.rate_limit.as_ref() else {
            return Ok(());
        };
        if rule.strategy != crate::domain::model::RateLimitStrategy::Reject {
            return Ok(());
        }
        let key = crate::domain::ratelimit::bucket_key(
            rule,
            resolved.upstream.id,
            resolved.route.map_id(),
            tenant_id,
            subject_id,
            client_ip,
        );
        match self.rate_limiter.check(&key, rule, now_ms) {
            crate::domain::ratelimit::Decision::Allowed { .. } => Ok(()),
            crate::domain::ratelimit::Decision::Limited {
                retry_after_seconds,
            } => Err(DomainError::new(
                ErrorKind::RateLimit,
                "rate limit exceeded for this upstream",
            )
            .with_alias(&resolved.upstream.alias)
            .with_retry_after(retry_after_seconds.max(1))),
        }
    }

    // -- validation ---------------------------------------------------------

    /// Validate an upstream draft.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::Validation`] with the reason the draft is not acceptable.
    pub fn validate_upstream(&self, upstream: &Upstream) -> Result<(), DomainError> {
        validate_endpoints(upstream)?;
        if let Some(auth) = &upstream.auth {
            validate_auth_config(auth, &self.plugins)?;
        }
        for binding in &upstream.plugins.items {
            validate_plugin_binding(&self.plugins, &self.store, binding)?;
        }
        if let Some(cors) = &upstream.cors {
            validate_cors(cors)?;
        }
        if let Some(limit) = &upstream.rate_limit {
            validate_rate_limit(limit)?;
        }
        for tag in &upstream.tags {
            validate_tag(tag)?;
        }
        Ok(())
    }

    /// Validate a route draft.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::Validation`] when the route is not acceptable, including
    /// when its `upstream_id` names an upstream of another tenant.
    pub fn validate_route(&self, route: &Route, tenant_id: &Uuid) -> Result<(), DomainError> {
        let owned = self
            .store
            .get_upstream(&route.upstream_id)
            .is_some_and(|upstream| &upstream.tenant_id == tenant_id);
        if !owned {
            return Err(DomainError::new(
                ErrorKind::Validation,
                format!(
                    "upstream_id {:?} does not name an upstream of this tenant",
                    instance_id(ids::UPSTREAM_RESOURCE_TYPE, route.upstream_id)
                ),
            ));
        }
        match &route.match_rule {
            MatchRule::Http(http) => {
                if http.methods.is_empty() {
                    return Err(validation(
                        "`match.http.methods` must list at least one method",
                    ));
                }
                if http.path.trim().is_empty() {
                    return Err(validation("`match.http.path` must not be empty"));
                }
                if !http.path.starts_with('/') {
                    return Err(validation("`match.http.path` must start with `/`"));
                }
                for method in &http.methods {
                    if method.trim().parse::<http::Method>().is_err() {
                        return Err(validation(format!("unsupported method {method:?}")));
                    }
                }
                for name in &http.query_allowlist {
                    if name.trim().is_empty() {
                        return Err(validation(
                            "`match.http.query_allowlist` must not contain empty names",
                        ));
                    }
                }
            }
            MatchRule::Grpc(grpc) => {
                if grpc.service.trim().is_empty() || grpc.method.trim().is_empty() {
                    return Err(validation(
                        "`match.grpc.service` and `match.grpc.method` are both required",
                    ));
                }
            }
        }
        for binding in &route.plugins.items {
            validate_plugin_binding(&self.plugins, &self.store, binding)?;
        }
        if let Some(limit) = &route.rate_limit {
            validate_rate_limit(limit)?;
        }
        for tag in &route.tags {
            validate_tag(tag)?;
        }
        Ok(())
    }

    /// Whether another route of the same tenant and upstream claims the same
    /// match rule.
    fn route_duplicated(&self, route: &Route) -> bool {
        self.store
            .routes_for_upstream(&route.tenant_id, &route.upstream_id)
            .iter()
            .any(|existing| existing.id != route.id && same_match_rule(existing, route))
    }

    /// Reject a request whose query parameters are not on the matched route's
    /// allowlist.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::Validation`] naming the first unexpected parameter.
    fn validate_query(&self, route: &Route, raw_query: Option<&str>) -> Result<(), DomainError> {
        let MatchRule::Http(http) = &route.match_rule else {
            return Ok(());
        };
        let query = OutboundQuery::parse(raw_query);
        if http.query_allowlist.is_empty() {
            if !query.all().is_empty() {
                return Err(DomainError::new(
                    ErrorKind::Validation,
                    "the matched route does not allow query parameters",
                ));
            }
            return Ok(());
        }
        for name in query.names() {
            if !http
                .query_allowlist
                .iter()
                .any(|allowed| allowed.eq_ignore_ascii_case(name))
            {
                return Err(DomainError::new(
                    ErrorKind::Validation,
                    format!("query parameter {name:?} is not on the route's allowlist"),
                ));
            }
        }
        Ok(())
    }
}

impl Route {
    /// The route's id for a rate-limit bucket key. A gRPC match is a catalog
    /// entry with no proxy path, so it contributes no bucket of its own.
    #[must_use]
    pub fn map_id(&self) -> Option<Uuid> {
        match &self.match_rule {
            MatchRule::Http(_) => Some(self.id),
            MatchRule::Grpc(_) => None,
        }
    }
}

/// `true` when every endpoint of the upstream is a TLS scheme.
fn endpoint_schemes_are_tls(upstream: &Upstream) -> bool {
    upstream
        .server
        .endpoints
        .iter()
        .all(|endpoint| endpoint.scheme != Scheme::Http)
}

/// `true` when the route's path prefix covers the requested suffix.
///
/// The proxy URL is `/oagw/v1/proxy/{alias}[/{path_suffix}]`, so the suffix is
/// what the caller of the route's `match.http.path` prefix has to cover.
fn prefix_matches(http: &HttpMatch, path_suffix: Option<&str>) -> bool {
    let prefix = http.path.trim_end_matches('/').trim_start_matches('/');
    if prefix.is_empty() {
        // A route with path `/` matches every request.
        return true;
    }
    let Some(suffix) = path_suffix else {
        return false;
    };
    let suffix = suffix.trim_matches('/');
    suffix.is_empty() || suffix == prefix || suffix.starts_with(&format!("{prefix}/"))
}

/// Compute the outbound path from the matched route and the request's suffix.
///
/// A route's `match.http.path` is a prefix of the proxied path: the client asks
/// for `/proxy/{alias}/{path_suffix}` and the suffix already starts with the
/// route path it matched, so the upstream receives the path the client asked
/// for. A `disabled` suffix mode serves the route path alone.
#[must_use]
fn outbound_path(route: &Route, suffix: Option<&str>) -> String {
    let MatchRule::Http(http) = &route.match_rule else {
        return String::new();
    };
    let base = http.path.trim_end_matches('/');
    let suffix = suffix.unwrap_or_default().trim_matches('/');
    if suffix.is_empty() || http.path_suffix_mode == PathSuffixMode::Disabled {
        return if base.is_empty() {
            "/".to_owned()
        } else {
            base.to_owned()
        };
    }
    format!("/{suffix}")
}

/// Validate the `X-OAGW-Target-Host` header: a bare hostname or IP — no port,
/// no path, no special characters.
fn validate_target_host(host: &str) -> Result<(), String> {
    if host.is_empty() {
        return Err("it is empty".to_owned());
    }
    if host.contains(['/', ':', '?', '#', '@', '\\', '%']) {
        return Err("it carries a port, path or other special characters".to_owned());
    }
    if crate::domain::alias::is_ip_literal(host) {
        return Ok(());
    }
    crate::domain::alias::validate_hostname(host)
}

/// The endpoint hosts a client may name in `X-OAGW-Target-Host`, as JSON.
fn valid_hosts(endpoints: &[Endpoint]) -> serde_json::Value {
    serde_json::Value::Array(
        endpoints
            .iter()
            .map(|endpoint| {
                serde_json::Value::String(crate::domain::alias::normalize(&endpoint.host))
            })
            .collect(),
    )
}

/// The endpoint hosts, as the comma-separated list ADR 0007 shows in `detail`.
fn hosts_csv(endpoints: &[Endpoint]) -> String {
    endpoints
        .iter()
        .map(|endpoint| crate::domain::alias::normalize(&endpoint.host))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Derive the alias an upstream's endpoints imply, or require an explicit one.
///
/// # Errors
///
/// [`ErrorKind::Validation`] when the endpoints cannot derive an alias and
/// none was supplied, or when a supplied alias disagrees with the derived one.
fn derive_or_require(upstream: &Upstream) -> Result<String, DomainError> {
    let supplied = upstream.alias.trim();
    let derived = crate::domain::alias::derive(&upstream.server.endpoints);
    match (derived, supplied.is_empty()) {
        (Some(derived), true) => Ok(derived),
        (Some(derived), false) => {
            let normalized = crate::domain::alias::normalize(supplied);
            if normalized.eq_ignore_ascii_case(&derived) {
                Ok(derived)
            } else {
                Err(DomainError::new(
                    ErrorKind::Validation,
                    format!(
                        "alias {normalized:?} was supplied for hostname endpoints that derive \
                         {derived:?}; hostname-based endpoints always auto-derive their alias"
                    ),
                ))
            }
        }
        (None, true) => Err(validation(
            "an explicit `alias` is required for IP-based or non-derivable endpoints",
        )),
        (None, false) => {
            crate::domain::alias::validate_hostname(supplied).map_err(|reason| {
                DomainError::new(
                    ErrorKind::Validation,
                    format!("alias {supplied:?} is not valid: {reason}"),
                )
            })?;
            Ok(crate::domain::alias::normalize(supplied))
        }
    }
}

/// Reject an update that would move the alias.
///
/// # Errors
///
/// [`ErrorKind::Validation`] when the recomputed alias differs from the
/// existing one.
fn enforce_alias_update(existing: &Upstream, replacement: &Upstream) -> Result<(), DomainError> {
    let replacement_alias = derive_or_require(replacement)?;
    if replacement_alias.eq_ignore_ascii_case(&existing.alias) {
        return Ok(());
    }
    Err(DomainError::new(
        ErrorKind::Validation,
        format!(
            "these endpoints would derive alias {replacement_alias:?}, but this upstream is \
             registered as {:?}; the alias is immutable once set, so delete and re-create it",
            existing.alias
        ),
    ))
}

/// Validate the endpoint pool.
fn validate_endpoints(upstream: &Upstream) -> Result<(), DomainError> {
    if upstream.server.endpoints.is_empty() {
        return Err(validation(
            "`server.endpoints` must list at least one endpoint",
        ));
    }
    for endpoint in &upstream.server.endpoints {
        if endpoint.host.trim().is_empty() {
            return Err(validation("an endpoint `host` must not be empty"));
        }
        if !crate::domain::alias::is_ip_literal(&endpoint.host) {
            crate::domain::alias::validate_hostname(&endpoint.host).map_err(|reason| {
                DomainError::new(
                    ErrorKind::Validation,
                    format!("endpoint host {:?} is not valid: {reason}", endpoint.host),
                )
            })?;
        }
        if endpoint.port == 0 {
            return Err(validation("an endpoint `port` must be between 1 and 65535"));
        }
    }
    // A pool is addressed through one alias, so every member must agree on the
    // scheme and the port the alias is derived from.
    let first = &upstream.server.endpoints[0];
    for endpoint in &upstream.server.endpoints[1..] {
        if endpoint.scheme != first.scheme {
            return Err(validation(
                "every endpoint in a pool must use the same scheme (cannot mix https and wss)",
            ));
        }
        if endpoint.port != first.port {
            return Err(validation(
                "every endpoint of an upstream must use the same port",
            ));
        }
    }
    Ok(())
}

/// Validate an `auth` block.
fn validate_auth_config(
    auth: &crate::domain::model::AuthConfig,
    plugins: &ControlPlane,
) -> Result<(), DomainError> {
    if auth.auth_type.trim().is_empty() {
        return Err(validation("`auth.type` must name an auth plugin"));
    }
    if ids::is_builtin(&auth.auth_type) {
        if ids::is_bindable(&auth.auth_type) {
            return Ok(());
        }
        return Err(DomainError::new(
            ErrorKind::Validation,
            format!(
                "auth plugin {:?} exists in the catalog but has no implementation and cannot be bound",
                auth.auth_type
            ),
        ));
    }
    if plugins.has_auth(&auth.auth_type) {
        return Ok(());
    }
    Err(DomainError::new(
        ErrorKind::Validation,
        format!("auth plugin {:?} is unknown", auth.auth_type),
    ))
}

/// Validate a `plugins.items[]` binding.
fn validate_plugin_binding(
    plugins: &ControlPlane,
    store: &SharedStore,
    binding: &PluginBinding,
) -> Result<(), DomainError> {
    if ids::is_builtin(&binding.plugin_ref) {
        if ids::is_bindable(&binding.plugin_ref) {
            return Ok(());
        }
        return Err(DomainError::new(
            ErrorKind::Validation,
            format!(
                "plugin {:?} is a catalog entry only and cannot be bound through plugins.items",
                binding.plugin_ref
            ),
        ));
    }
    if let Some(id) = parse_instance_id(&binding.plugin_ref) {
        let Some(definition) = store.get_plugin(&id) else {
            return Err(DomainError::new(
                ErrorKind::Validation,
                format!(
                    "plugin {:?} is not a known plugin definition",
                    binding.plugin_ref
                ),
            ));
        };
        // `plugins.items[]` holds guard and transform plugins only; an auth
        // plugin is configured through the upstream's `auth` block.
        if definition.plugin_type == PluginType::Auth {
            return Err(DomainError::new(
                ErrorKind::Validation,
                format!(
                    "plugin {:?} is an auth plugin and is configured through `auth`, not `plugins.items`",
                    binding.plugin_ref
                ),
            ));
        }
        if definition.plugin_ref != binding.plugin_ref {
            return Err(DomainError::new(
                ErrorKind::Validation,
                format!(
                    "plugin {:?} does not name the definition registered as {:?}",
                    binding.plugin_ref, definition.plugin_ref
                ),
            ));
        }
        return Ok(());
    }
    if plugins.knows(&binding.plugin_ref) {
        return Ok(());
    }
    Err(DomainError::new(
        ErrorKind::Validation,
        format!("plugin {:?} is not registered", binding.plugin_ref),
    ))
}

/// Validate the `config` payload of a custom plugin definition.
///
/// This release executes a definition's `config` as a declarative document (see
/// [`crate::infra::plugins::DefinitionPlugin`]), so the document must be one
/// this gear can run: a transform definition carries a header transform, a
/// guard definition names at least one required header, and an auth definition
/// carries the credential reference its plugin will resolve.
fn validate_plugin_config(draft: &PluginDefinition) -> Result<(), DomainError> {
    use crate::infra::plugins::DefinitionPlugin;
    let executable = match draft.plugin_type {
        PluginType::Transform => {
            let transform: crate::domain::model::HeaderTransform =
                serde_json::from_value(draft.config.clone()).map_err(|err| {
                    validation(format!("plugin `config` is not a header transform: {err}"))
                })?;
            !transform.set.is_empty() || !transform.add.is_empty() || !transform.remove.is_empty()
        }
        PluginType::Guard => !DefinitionPlugin::guard(draft).is_inert(),
        PluginType::Auth => draft.config.get("secret_ref").is_some_and(Value::is_string),
    };
    if executable {
        return Ok(());
    }
    Err(validation(
        "this release runs a plugin definition's `config` document as data; it must name \
         at least one operation (a transform's `set`/`add`/`remove`, a guard's \
         `required_request_headers`/`required_response_headers`, or an auth `secret_ref`)",
    ))
}

/// Validate a CORS block (ADR 0004).
fn validate_cors(cors: &CorsRule) -> Result<(), DomainError> {
    if cors.allow_credentials && cors.allowed_origins.iter().any(|origin| origin == "*") {
        return Err(validation(
            "cors.allow_credentials cannot be combined with the `*` origin",
        ));
    }
    Ok(())
}

/// Validate a rate-limit rule (ADR 0003).
fn validate_rate_limit(limit: &RateLimitRule) -> Result<(), DomainError> {
    if limit.sustained.rate == 0 {
        return Err(validation("`rate_limit.sustained.rate` must be at least 1"));
    }
    if limit
        .burst
        .as_ref()
        .is_some_and(|burst| burst.capacity == 0)
    {
        return Err(validation("`rate_limit.burst.capacity` must be at least 1"));
    }
    if limit.cost == 0 {
        return Err(validation("`rate_limit.cost` must be at least 1"));
    }
    Ok(())
}

/// Validate a tag.
fn validate_tag(tag: &str) -> Result<(), DomainError> {
    if tag.is_empty() {
        return Err(validation("a tag must not be empty"));
    }
    if !tag.bytes().all(|byte| {
        byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_' || byte == b'-'
    }) {
        return Err(DomainError::new(
            ErrorKind::Validation,
            format!("tag {tag:?} is not valid; tags are lowercase letters, digits, `-` and `_`"),
        ));
    }
    Ok(())
}

/// `true` when two routes claim the same method and path.
fn same_match_rule(left: &Route, right: &Route) -> bool {
    match (&left.match_rule, &right.match_rule) {
        (MatchRule::Http(a), MatchRule::Http(b)) => {
            a.path == b.path && a.methods.iter().any(|method| b.methods.contains(method))
        }
        (MatchRule::Grpc(a), MatchRule::Grpc(b)) => a.service == b.service && a.method == b.method,
        _ => false,
    }
}

fn validation(detail: impl Into<String>) -> DomainError {
    DomainError::new(ErrorKind::Validation, detail)
}

fn conflict(detail: impl Into<String>) -> DomainError {
    DomainError::new(ErrorKind::Conflict, detail)
}

fn not_found(kind: &str, id: &Uuid) -> DomainError {
    DomainError::new(
        ErrorKind::RouteNotFound,
        format!("no {kind} with id {id} is visible to this tenant"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::model::{Endpoint, HttpMatch, Protocol, ServerConfig};
    use crate::domain::ratelimit::RateLimiter;
    use crate::domain::store::Store;
    use credstore_sdk::test_util::MockCredStoreClient;
    use toolkit_security::SecurityContext;

    fn tenant() -> Uuid {
        Uuid::new_v4()
    }

    fn security(tenant_id: Uuid) -> SecurityContext {
        SecurityContext::builder()
            .subject_id(Uuid::new_v4())
            .subject_tenant_id(tenant_id)
            .build()
            .expect("a security context")
    }

    fn service() -> Arc<Service> {
        let credential_store: SharedCredentialStore =
            Arc::new(MockCredStoreClient::with_secrets(Vec::new()));
        Service::new(
            Store::new(),
            Arc::new(ControlPlane::with_builtins(
                credential_store.clone(),
                Arc::new(crate::infra::token_cache::TokenCache::new(64)),
            )),
            credential_store,
            None,
            OagwConfig {
                allow_http_upstream: true,
                ..OagwConfig::default()
            },
            Arc::new(RateLimiter::new()),
        )
    }

    // A hostname upstream on a standard port: the alias is derived from the
    // endpoint (DESIGN §3.2), so the fixture leaves it blank and lets the
    // service derive it.
    fn upstream(tenant_id: Uuid, alias: &str, host: &str, port: u16) -> Upstream {
        Upstream {
            id: Uuid::new_v4(),
            tenant_id,
            enabled: true,
            alias: alias.to_owned(),
            tags: Vec::new(),
            server: ServerConfig {
                endpoints: vec![Endpoint {
                    scheme: Scheme::Http,
                    host: host.to_owned(),
                    port,
                }],
            },
            protocol: Protocol::Http,
            auth: None,
            headers: Default::default(),
            plugins: Default::default(),
            rate_limit: None,
            cors: None,
            created_at: None,
            updated_at: None,
        }
    }

    fn rate_limit(rate: u64) -> RateLimitRule {
        RateLimitRule {
            sharing: Sharing::Private,
            algorithm: crate::domain::model::RateLimitAlgorithm::SlidingWindow,
            sustained: crate::domain::model::Sustained {
                rate,
                window: crate::domain::model::RateLimitWindow::Minute,
            },
            burst: None,
            scope: crate::domain::model::RateLimitScope::Tenant,
            strategy: crate::domain::model::RateLimitStrategy::Reject,
            cost: 1,
        }
    }

    fn plugin_binding(plugin_ref: &str) -> PluginBinding {
        PluginBinding {
            plugin_ref: plugin_ref.to_owned(),
            config: None,
        }
    }

    fn route(tenant_id: Uuid, upstream_id: Uuid, methods: &[&str], path: &str) -> Route {
        Route {
            id: Uuid::new_v4(),
            tenant_id,
            enabled: true,
            tags: Vec::new(),
            upstream_id,
            match_rule: MatchRule::Http(HttpMatch {
                methods: methods.iter().map(|method| (*method).to_owned()).collect(),
                path: path.to_owned(),
                query_allowlist: Vec::new(),
                path_suffix_mode: Default::default(),
            }),
            plugins: Default::default(),
            rate_limit: None,
            created_at: None,
            updated_at: None,
        }
    }

    async fn created_upstream(tenant_id: Uuid) -> (Arc<Service>, Upstream) {
        let service = service();
        let draft = upstream(tenant_id, "", "api.example.test", 80);
        (
            service.clone(),
            service
                .create_upstream(&security(tenant_id), draft)
                .await
                .expect("create"),
        )
    }

    #[test]
    fn instance_ids_round_trip_through_their_reference_form() {
        let id = Uuid::new_v4();
        let reference = instance_id(ids::UPSTREAM_RESOURCE_TYPE, id);
        assert!(reference.starts_with(ids::UPSTREAM_RESOURCE_TYPE));
        assert_eq!(parse_instance_id(&reference), Some(id));
        assert_eq!(parse_instance_id(&id.to_string()), Some(id));
        assert_eq!(parse_instance_id("not-an-id"), None);
    }

    #[tokio::test]
    async fn a_created_upstream_is_tenant_scoped_and_gettable() {
        let tenant_id = tenant();
        let (service, created) = created_upstream(tenant_id).await;
        assert_eq!(created.tenant_id, tenant_id);
        assert!(created.created_at.is_some());
        assert!(service.get_upstream(&tenant_id, &created.id).is_ok());
        // Another tenant cannot see it.
        assert_eq!(
            service
                .get_upstream(&tenant(), &created.id)
                .unwrap_err()
                .kind(),
            ErrorKind::RouteNotFound
        );
        assert!(
            service
                .list_upstreams(&tenant(), &Default::default())
                .is_empty()
        );
    }

    #[tokio::test]
    async fn a_duplicate_alias_conflicts() {
        let tenant_id = tenant();
        let (service, _) = created_upstream(tenant_id).await;
        let draft = upstream(tenant_id, "API.Example.Test", "api.example.test", 80);
        assert_eq!(
            service
                .create_upstream(&security(tenant_id), draft)
                .await
                .unwrap_err()
                .kind(),
            ErrorKind::Conflict
        );
    }

    #[tokio::test]
    async fn an_upstream_without_endpoints_is_refused() {
        let tenant_id = tenant();
        let service = service();
        let mut draft = upstream(tenant_id, "api.example.test", "api.example.test", 443);
        draft.server.endpoints.clear();
        assert_eq!(
            service
                .create_upstream(&security(tenant_id), draft)
                .await
                .unwrap_err()
                .kind(),
            ErrorKind::Validation
        );
    }

    #[tokio::test]
    async fn replacing_an_upstream_keeps_its_identity() {
        let tenant_id = tenant();
        let (service, created) = created_upstream(tenant_id).await;
        let mut replacement = upstream(tenant_id, "", "api.example.test", 80);
        replacement.tags = vec!["rotated".to_owned()];
        let updated = service
            .replace_upstream(&security(tenant_id), &created.id, replacement)
            .await
            .expect("replace");
        assert_eq!(updated.id, created.id);
        assert_eq!(updated.created_at, created.created_at);
        assert!(updated.updated_at.is_some());
        assert_eq!(
            service
                .get_upstream(&tenant_id, &created.id)
                .expect("get")
                .tags,
            vec!["rotated"]
        );
    }

    #[tokio::test]
    async fn an_upstream_referenced_by_a_route_cannot_be_deleted() {
        let tenant_id = tenant();
        let (service, created) = created_upstream(tenant_id).await;
        service
            .create_route(
                &security(tenant_id),
                route(tenant_id, created.id, &["GET"], "/v1"),
            )
            .expect("route");
        assert_eq!(
            service
                .delete_upstream(&tenant_id, &created.id)
                .unwrap_err()
                .kind(),
            ErrorKind::PluginInUse
        );
    }

    #[tokio::test]
    async fn an_unreferenced_upstream_deletes() {
        let tenant_id = tenant();
        let (service, created) = created_upstream(tenant_id).await;
        assert!(service.delete_upstream(&tenant_id, &created.id).is_ok());
        assert_eq!(
            service
                .get_upstream(&tenant_id, &created.id)
                .unwrap_err()
                .kind(),
            ErrorKind::RouteNotFound
        );
    }

    #[tokio::test]
    async fn a_route_must_reference_an_upstream_of_its_own_tenant() {
        let owner = tenant();
        let other = tenant();
        let (service, created) = created_upstream(owner).await;
        assert_eq!(
            service
                .create_route(&security(other), route(other, created.id, &["GET"], "/v1"))
                .unwrap_err()
                .kind(),
            ErrorKind::Validation
        );
    }

    #[tokio::test]
    async fn a_duplicate_route_match_conflicts() {
        let tenant_id = tenant();
        let (service, created) = created_upstream(tenant_id).await;
        let security = security(tenant_id);
        service
            .create_route(&security, route(tenant_id, created.id, &["GET"], "/v1"))
            .expect("first");
        assert_eq!(
            service
                .create_route(&security, route(tenant_id, created.id, &["GET"], "/v1"))
                .unwrap_err()
                .kind(),
            ErrorKind::Conflict
        );
    }

    #[tokio::test]
    async fn resolution_picks_the_route_and_the_endpoint() {
        let tenant_id = tenant();
        let (service, created) = created_upstream(tenant_id).await;
        let mut permitted = route(tenant_id, created.id, &["GET", "POST"], "/v1");
        permitted.match_rule = MatchRule::Http(HttpMatch {
            methods: vec!["GET".to_owned(), "POST".to_owned()],
            path: "/v1".to_owned(),
            query_allowlist: vec!["model".to_owned()],
            path_suffix_mode: Default::default(),
        });
        service
            .create_route(&security(tenant_id), permitted)
            .expect("route");

        let resolved = service
            .resolve_proxy(
                &security(tenant_id),
                "api.example.test",
                Some("v1/chat/completions"),
                &http::Method::POST,
                Some("model=gpt-4"),
                "127.0.0.1",
                None,
            )
            .await
            .expect("resolve");
        assert_eq!(resolved.upstream.id, created.id);
        // The suffix is the path the upstream sees, prefix included.
        assert_eq!(resolved.outbound_path, "/v1/chat/completions");
        assert_eq!(resolved.endpoint.host, "api.example.test");
        assert_eq!(resolved.effective.rate_limit, None);
    }

    #[tokio::test]
    async fn an_unknown_alias_is_route_not_found() {
        let service = service();
        let error = service
            .resolve_proxy(
                &security(tenant()),
                "no-such-alias.test",
                None,
                &http::Method::GET,
                None,
                "",
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::RouteNotFound);
        assert_eq!(error.status(), 404);
    }

    #[tokio::test]
    async fn a_request_without_a_matching_route_is_refused() {
        let tenant_id = tenant();
        let (service, created) = created_upstream(tenant_id).await;
        service
            .create_route(
                &security(tenant_id),
                route(tenant_id, created.id, &["GET"], "/v1"),
            )
            .expect("route");
        let error = service
            .resolve_proxy(
                &security(tenant_id),
                "api.example.test",
                Some("v1/other"),
                &http::Method::DELETE,
                None,
                "",
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::RouteNotFound);
    }

    #[tokio::test]
    async fn a_disabled_upstream_is_link_unavailable() {
        let tenant_id = tenant();
        let service = service();
        let mut draft = upstream(tenant_id, "", "api.example.test", 80);
        draft.enabled = false;
        let created = service
            .create_upstream(&security(tenant_id), draft)
            .await
            .expect("create");
        service
            .create_route(
                &security(tenant_id),
                route(tenant_id, created.id, &["GET"], "/v1"),
            )
            .expect("route");
        let error = service
            .resolve_proxy(
                &security(tenant_id),
                "api.example.test",
                Some("v1"),
                &http::Method::GET,
                None,
                "",
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::LinkUnavailable);
        assert_eq!(error.status(), 503);
    }

    #[tokio::test]
    async fn a_target_host_that_is_not_a_bare_host_is_invalid() {
        let tenant_id = tenant();
        let (service, created) = created_upstream(tenant_id).await;
        service
            .create_route(
                &security(tenant_id),
                route(tenant_id, created.id, &["GET"], "/"),
            )
            .expect("route");
        let error = service
            .resolve_proxy(
                &security(tenant_id),
                "api.example.test",
                Some("v1"),
                &http::Method::GET,
                None,
                "",
                Some("api.example.test:8080"),
            )
            .await
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::InvalidTargetHost);
        assert_eq!(error.status(), 400);
    }

    #[tokio::test]
    async fn an_unknown_target_host_lists_the_valid_ones() {
        let tenant_id = tenant();
        let (service, created) = created_upstream(tenant_id).await;
        service
            .create_route(
                &security(tenant_id),
                route(tenant_id, created.id, &["GET"], "/"),
            )
            .expect("route");
        let error = service
            .resolve_proxy(
                &security(tenant_id),
                "api.example.test",
                Some("v1"),
                &http::Method::GET,
                None,
                "",
                Some("other.example.test"),
            )
            .await
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::UnknownTargetHost);
        assert_eq!(
            error.problem_body().get("valid_hosts"),
            Some(&serde_json::json!(["api.example.test"]))
        );
    }

    #[tokio::test]
    async fn the_strictest_rate_limit_of_the_hierarchy_is_enforced() {
        let tenant_id = tenant();
        let mut limit = rate_limit(100);
        limit.sharing = Sharing::Enforce;
        let service = service();
        let mut draft = upstream(tenant_id, "", "api.example.test", 80);
        draft.rate_limit = Some(limit);
        let created = service
            .create_upstream(&security(tenant_id), draft)
            .await
            .expect("create");
        let mut child = route(tenant_id, created.id, &["GET"], "/v1");
        child.rate_limit = Some(rate_limit(10));
        service
            .create_route(&security(tenant_id), child)
            .expect("route");

        let resolved = service
            .resolve_proxy(
                &security(tenant_id),
                "api.example.test",
                Some("v1"),
                &http::Method::GET,
                None,
                "",
                None,
            )
            .await
            .expect("resolve");
        let capacity = resolved
            .effective
            .rate_limit
            .as_ref()
            .map(|rule| rule.capacity());
        // The route's 10/minute is stricter than the upstream's enforced 100.
        assert_eq!(capacity, Some(10));
        for _ in 0..10 {
            service
                .check_rate_limit(&resolved, tenant_id, Uuid::new_v4(), "127.0.0.1", 0)
                .expect("admitted");
        }
        let error = service
            .check_rate_limit(&resolved, tenant_id, Uuid::new_v4(), "127.0.0.1", 0)
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::RateLimit);
        assert_eq!(error.status(), 429);
        assert!(error.problem_body().contains_key("retry_after_seconds"));
    }

    #[tokio::test]
    async fn an_enforced_ancestor_rate_limit_survives_a_looser_child() {
        let tenant_id = tenant();
        let mut limit = rate_limit(5);
        limit.sharing = Sharing::Enforce;
        let mut draft = upstream(tenant_id, "", "api.example.test", 80);
        draft.rate_limit = Some(limit);
        let service = service();
        let created = service
            .create_upstream(&security(tenant_id), draft)
            .await
            .expect("create");
        let mut child = route(tenant_id, created.id, &["GET"], "/v1");
        child.rate_limit = Some(rate_limit(10_000));
        service
            .create_route(&security(tenant_id), child)
            .expect("route");
        let resolved = service
            .resolve_proxy(
                &security(tenant_id),
                "api.example.test",
                Some("v1"),
                &http::Method::GET,
                None,
                "",
                None,
            )
            .await
            .expect("resolve");
        assert_eq!(
            resolved.effective.rate_limit.expect("a limit").capacity(),
            5
        );
    }

    #[tokio::test]
    async fn plugins_are_chained_ancestor_first() {
        let tenant_id = tenant();
        let service = service();
        let mut ancestor = upstream(tenant_id, "", "api.example.test", 80);
        ancestor.plugins.sharing = Sharing::Enforce;
        ancestor.plugins.items = vec![plugin_binding(ids::AUTH_PLUGIN_APIKEY)];
        let created = service
            .create_upstream(&security(tenant_id), ancestor)
            .await
            .expect("create");
        let mut child = route(tenant_id, created.id, &["GET"], "/v1");
        child.plugins.items = vec![plugin_binding(ids::GUARD_PLUGIN_REQUIRED_HEADERS)];
        service
            .create_route(&security(tenant_id), child)
            .expect("route");
        let resolved = service
            .resolve_proxy(
                &security(tenant_id),
                "api.example.test",
                Some("v1"),
                &http::Method::GET,
                None,
                "",
                None,
            )
            .await
            .expect("resolve");
        let refs: Vec<&str> = resolved
            .effective
            .plugins
            .iter()
            .map(|binding| binding.plugin_ref.as_str())
            .collect();
        // The enforced ancestor binding runs first, the route's own second.
        assert_eq!(
            refs,
            vec![ids::AUTH_PLUGIN_APIKEY, ids::GUARD_PLUGIN_REQUIRED_HEADERS]
        );
    }

    #[tokio::test]
    async fn plaintext_endpoints_are_refused_when_the_flag_is_off() {
        let tenant_id = tenant();
        let credential_store: SharedCredentialStore =
            Arc::new(MockCredStoreClient::with_secrets(Vec::new()));
        let service = Service::new(
            Store::new(),
            Arc::new(ControlPlane::with_builtins(
                credential_store.clone(),
                Arc::new(crate::infra::token_cache::TokenCache::new(64)),
            )),
            credential_store,
            None,
            OagwConfig::default(),
            Arc::new(RateLimiter::new()),
        );
        let draft = upstream(tenant_id, "", "api.example.test", 80);
        let created = service
            .create_upstream(&security(tenant_id), draft)
            .await
            .expect("create");
        service
            .create_route(
                &security(tenant_id),
                route(tenant_id, created.id, &["GET"], "/v1"),
            )
            .expect("route");
        assert_eq!(
            service
                .resolve_proxy(
                    &security(tenant_id),
                    "api.example.test",
                    Some("v1"),
                    &http::Method::GET,
                    None,
                    "",
                    None
                )
                .await
                .unwrap_err()
                .kind(),
            ErrorKind::LinkUnavailable
        );
    }

    #[test]
    fn a_query_outside_the_route_allowlist_is_refused() {
        let tenant_id = tenant();
        let service = service();
        let mut child = route(tenant_id, Uuid::new_v4(), &["GET"], "/v1");
        child.match_rule = MatchRule::Http(HttpMatch {
            methods: vec!["GET".to_owned()],
            path: "/v1".to_owned(),
            query_allowlist: vec!["model".to_owned()],
            path_suffix_mode: Default::default(),
        });
        assert!(service.validate_query(&child, Some("model=gpt-4")).is_ok());
        assert_eq!(
            service
                .validate_query(&child, Some("model=gpt-4&tool=1"))
                .unwrap_err()
                .kind(),
            ErrorKind::Validation
        );
    }

    // -- custom plugin definitions -----------------------------------------

    #[tokio::test]
    async fn a_custom_definition_resolves_out_of_the_store() {
        let tenant_id = tenant();
        let service = service();
        let draft = PluginDefinition {
            id: Uuid::nil(),
            tenant_id: Uuid::nil(),
            plugin_ref: String::new(),
            plugin_type: PluginType::Transform,
            name: "add-header".to_owned(),
            description: String::new(),
            config: serde_json::json!({"add": {"x-custom": "yes"}}),
            config_schema: None,
            source_code: None,
            version: 1,
            enabled: true,
            created_at: None,
        };
        let created = service
            .create_plugin(&tenant_id, draft)
            .expect("a transform definition with an operation is accepted");

        // A UUID-backed ref resolves out of the definition store, not the
        // registry — and yields a plugin carrying the definition's config.
        let binding = plugin_binding(&created.plugin_ref);
        let plugin = service
            .transform_plugin(&binding)
            .expect("the ref resolves")
            .expect("a transform definition answers in the transform slot");
        assert_eq!(plugin.id(), created.plugin_ref);
        let mut headers = http::HeaderMap::new();
        plugin
            .transform_request(
                &crate::domain::plugin::ProxyContext {
                    security: toolkit_security::SecurityContext::anonymous(),
                    tenant_id,
                    alias: "api.example.test".to_owned(),
                    upstream_id: Uuid::nil(),
                    route_id: None,
                    endpoint_host: "api.example.test".to_owned(),
                    outbound_path: "/v1".to_owned(),
                    client_ip: "127.0.0.1".to_owned(),
                },
                &Value::Null,
                &mut headers,
            )
            .await
            .expect("the definition runs");
        assert_eq!(headers.get("x-custom").unwrap(), "yes");
    }

    #[tokio::test]
    async fn a_definition_is_only_run_in_its_own_slot() {
        let tenant_id = tenant();
        let service = service();
        let created = service
            .create_plugin(
                &tenant_id,
                PluginDefinition {
                    id: Uuid::nil(),
                    tenant_id: Uuid::nil(),
                    plugin_ref: String::new(),
                    plugin_type: PluginType::Transform,
                    name: "add-header".to_owned(),
                    description: String::new(),
                    config: serde_json::json!({"add": {"x-custom": "yes"}}),
                    config_schema: None,
                    source_code: None,
                    version: 1,
                    enabled: true,
                    created_at: None,
                },
            )
            .expect("create");
        let binding = plugin_binding(&created.plugin_ref);
        // The guard chain has nothing to run, but the ref still resolves.
        assert!(service.guard_plugin(&binding).expect("resolves").is_none());
        assert!(
            service
                .transform_plugin(&binding)
                .expect("resolves")
                .is_some()
        );
    }

    #[tokio::test]
    async fn an_unresolvable_custom_plugin_ref_is_plugin_not_found() {
        let service = service();
        let binding = plugin_binding(&format!(
            "{}0f0e0d0c-0b0a-49f8-8765-432109876543",
            ids::TRANSFORM_PLUGIN_RESOURCE_TYPE
        ));
        let error = match service.transform_plugin(&binding) {
            Ok(Some(_)) => panic!("the binding names no stored definition"),
            Ok(None) => panic!("a UUID-backed ref must resolve or fail, never vanish"),
            Err(error) => error,
        };
        assert_eq!(error.kind(), ErrorKind::PluginNotFound);
        assert_eq!(error.status(), 503);
    }

    #[test]
    fn a_definition_with_no_executable_configuration_is_refused() {
        let tenant_id = tenant();
        let service = service();
        let draft = PluginDefinition {
            id: Uuid::nil(),
            tenant_id: Uuid::nil(),
            plugin_ref: String::new(),
            plugin_type: PluginType::Transform,
            name: "starlark-only".to_owned(),
            description: String::new(),
            config: serde_json::json!({}),
            config_schema: None,
            source_code: Some("def transform_request(ctx, headers):\n    return None\n".to_owned()),
            version: 1,
            enabled: true,
            created_at: None,
        };
        let error = service.create_plugin(&tenant_id, draft).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Validation);
        assert!(
            error.to_string().contains("at least one operation"),
            "{error}"
        );
    }

    #[test]
    fn a_plugin_definition_serves_its_starlark_source() {
        let tenant_id = tenant();
        let service = service();
        let draft = PluginDefinition {
            id: Uuid::nil(),
            tenant_id: Uuid::nil(),
            plugin_ref: String::new(),
            plugin_type: PluginType::Guard,
            name: "require-tenant".to_owned(),
            description: String::new(),
            config: serde_json::json!({"required_request_headers": "x-tenant-id"}),
            config_schema: None,
            source_code: Some("def check_request(ctx, headers):\n    return None\n".to_owned()),
            version: 1,
            enabled: true,
            created_at: None,
        };
        let created = service
            .create_plugin(&tenant_id, draft)
            .expect("a guard definition naming a required header is accepted");
        let source = service
            .plugin_source(&tenant_id, &created.id)
            .expect("source");
        assert_eq!(
            source["source"],
            "def check_request(ctx, headers):\n    return None\n"
        );
        assert_eq!(source["id"], created.plugin_ref);
    }
}
