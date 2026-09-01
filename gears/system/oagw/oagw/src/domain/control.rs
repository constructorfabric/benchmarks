//! Control-plane service: CRUD + validation + alias enforcement for
//! upstreams, routes and plugins, plus the tenant-chain resolution used by
//! the data plane.

use std::collections::BTreeMap;
use std::sync::Arc;

use http::header::{HeaderName, HeaderValue};
use tenant_resolver_sdk::{GetAncestorsOptions, TenantId, TenantResolverClient};
use toolkit_security::SecurityContext;
use url::Url;
use uuid::Uuid;

use crate::domain::alias::{derive_alias, normalize_alias, validate_host};
use crate::domain::error::DomainError;
use crate::domain::model::{
    CorsConfig, Endpoint, HeadersConfig, HttpMatch, Plugin, PluginRecord, PluginsBinding,
    RateLimit, Route, RouteRecord, Upstream, UpstreamRecord,
};
use crate::domain::ratelimit::{RateLimiter, rate_scope_key};
use crate::domain::repo::OagwRepository;
use crate::gts;

/// A resolved upstream: the closest chain match plus every chain match (for
/// enforced-ancestor policy application).
#[derive(Debug, Clone)]
pub struct ResolvedUpstream {
    /// The closest (descendant-most) match.
    pub selected: UpstreamRecord,
    /// All chain matches ordered descendant -> root.
    pub chain: Vec<UpstreamRecord>,
}

/// Auth plugin type ids that are bindable as an upstream `auth.type`: every
/// implemented built-in auth plugin (the mirror of the data plane's auth
/// registry — all six resolve at request time). A config referencing any
/// other id is written with a guaranteed-irresolvable binding; rejecting it at
/// CRUD time turns the request-time `503`/`500` into an immediate `400`
/// (F-012 config-time validation).
static KNOWN_AUTH_PLUGIN_IDS: &[&str] = &[
    gts::AUTH_NOOP_ID,
    gts::AUTH_APIKEY_ID,
    gts::AUTH_OAUTH2_ID,
    gts::AUTH_OAUTH2_BASIC_ID,
    gts::AUTH_BASIC_ID,
    gts::AUTH_BEARER_ID,
];

/// Every plugin identifier that is bindable by name (implemented built-ins
/// plus catalog-only reserved ids); custom plugins resolve by UUID via the
/// repository.
static KNOWN_PLUGIN_IDS: &[&str] = &[
    gts::AUTH_NOOP_ID,
    gts::AUTH_APIKEY_ID,
    gts::AUTH_OAUTH2_ID,
    gts::AUTH_OAUTH2_BASIC_ID,
    gts::AUTH_BASIC_ID,
    gts::AUTH_BEARER_ID,
    gts::GUARD_REQUIRED_HEADERS_ID,
    gts::GUARD_TIMEOUT_ID,
    gts::GUARD_CORS_ID,
    gts::TRANSFORM_REQUEST_ID_ID,
    gts::TRANSFORM_LOGGING_ID,
    gts::TRANSFORM_METRICS_ID,
];

/// Control-plane service.
pub struct ControlPlaneService {
    repo: Arc<dyn OagwRepository>,
    tenants: Arc<dyn TenantResolverClient>,
    /// Data-plane rate limiter hook (attached at wiring time) so that
    /// deletions can evict scope buckets; `None` until the data plane starts.
    rate_limiter: parking_lot::RwLock<Option<Arc<RateLimiter>>>,
}

impl ControlPlaneService {
    /// Create a control-plane service.
    #[must_use]
    pub fn new(repo: Arc<dyn OagwRepository>, tenants: Arc<dyn TenantResolverClient>) -> Self {
        Self {
            repo,
            tenants,
            rate_limiter: parking_lot::RwLock::new(None),
        }
    }

    /// Attach the data-plane rate limiter (wiring time; idempotent).
    pub fn attach_rate_limiter(&self, limiter: Arc<RateLimiter>) {
        *self.rate_limiter.write() = Some(limiter);
    }

    /// Evict rate-limiter scope buckets for a deleted resource, based on the
    /// rate-limit configs that were active on it (global / tenant-scoped /
    /// route-scoped keys can be computed without live caller data).
    fn evict_rate_keys_with(&self, scopes: impl IntoIterator<Item = Option<String>>) {
        if let Some(limiter) = self.rate_limiter.read().as_ref() {
            for key in scopes.into_iter().flatten() {
                limiter.remove_key(&key);
            }
        }
    }

    /// Shared repository handle (used by the data plane and tests).
    #[must_use]
    pub fn repo(&self) -> Arc<dyn OagwRepository> {
        self.repo.clone()
    }

    // =======================================================================
    // Tenant hierarchy
    // =======================================================================

    /// Build the tenant chain `[self, parent, ..., root]` for the calling
    /// tenant.
    ///
    /// # Errors
    ///
    /// Returns [`DomainError::Internal`] when the tenant resolver cannot
    /// answer.
    pub async fn tenant_chain(
        &self,
        ctx: &SecurityContext,
        tenant_id: Uuid,
    ) -> Result<Vec<Uuid>, DomainError> {
        let resp = self
            .tenants
            .get_ancestors(ctx, TenantId(tenant_id), &GetAncestorsOptions::default())
            .await
            .map_err(|e| DomainError::internal(format!("tenant resolver get_ancestors: {e}")))?;
        let mut chain = vec![tenant_id];
        for ref_tenant in &resp.ancestors {
            chain.push(ref_tenant.id.0);
        }
        Ok(chain)
    }

    /// Resolve an alias across the tenant chain (descendant -> root).
    ///
    /// Returns `None` when no chain tenant owns an upstream with this alias.
    ///
    /// # Errors
    ///
    /// Returns [`DomainError::Validation`] for an invalid alias and
    /// [`DomainError::Internal`] when the tenant resolver fails.
    pub async fn resolve_alias(
        &self,
        ctx: &SecurityContext,
        tenant_id: Uuid,
        alias: &str,
    ) -> Result<Option<ResolvedUpstream>, DomainError> {
        let normalized = normalize_alias(alias).map_err(DomainError::validation)?;
        let chain = self.tenant_chain(ctx, tenant_id).await?;
        let all = self.repo.list_upstreams_for_tenants(&chain).await?;
        let mut matches: Vec<UpstreamRecord> = all
            .into_iter()
            .filter(|r| {
                if !r
                    .entity
                    .alias
                    .as_deref()
                    .is_some_and(|a| a.eq_ignore_ascii_case(&normalized))
                {
                    return false;
                }
                // `private`-shared capabilities make an ancestor entry opaque
                // to descendants: a child tenant must never resolve (and
                // therefore proxy) a private ancestor upstream.
                if r.tenant_id != tenant_id && r.entity.has_private_sharing() {
                    return false;
                }
                true
            })
            .collect();
        // Order by chain proximity: closest (descendant) tenant first.
        matches.sort_by_key(|r| {
            chain
                .iter()
                .position(|t| *t == r.tenant_id)
                .unwrap_or(usize::MAX)
        });
        let Some(selected) = matches.first().cloned() else {
            return Ok(None);
        };
        Ok(Some(ResolvedUpstream {
            selected,
            chain: matches,
        }))
    }

    // =======================================================================
    // Upstream CRUD
    // =======================================================================

    /// Create an upstream owned by `tenant_id`.
    ///
    /// # Errors
    ///
    /// Returns [`DomainError::Validation`] or [`DomainError::Conflict`] when
    /// the alias is already taken by the same tenant or reserved by a
    /// `private`-shared ancestor.
    pub async fn create_upstream(
        &self,
        ctx: &SecurityContext,
        tenant_id: Uuid,
        input: Upstream,
    ) -> Result<UpstreamRecord, DomainError> {
        let alias = Self::validate_and_assign_alias(input.clone(), None)?;
        self.validate_plugin_bindings(tenant_id, input.plugins.as_ref())
            .await?;
        self.check_shadow_allowed(ctx, tenant_id, &alias).await?;
        let mut entity = input;
        entity.alias = Some(alias);
        let id = Uuid::new_v4();
        entity.id = Some(gts::resource_instance_id(gts::UPSTREAM_TYPE_ID, &id));
        let record = UpstreamRecord { tenant_id, entity };
        self.repo.insert_upstream(tenant_id, record.clone()).await?;
        Ok(record)
    }

    /// Replace an upstream (full overwrite). The alias is immutable.
    ///
    /// # Errors
    ///
    /// Returns [`DomainError::NotFound`] when the caller does not own it and
    /// validation/conflict errors otherwise.
    pub async fn update_upstream(
        &self,
        ctx: &SecurityContext,
        tenant_id: Uuid,
        id: Uuid,
        input: Upstream,
    ) -> Result<UpstreamRecord, DomainError> {
        let existing = self.repo.get_upstream(tenant_id, id).await?;
        let old_alias = existing.entity.alias.clone().unwrap_or_default();
        if !Self::alias_transition_allowed(&existing.entity, &input)? {
            return Err(DomainError::validation(
                "alias is immutable; delete and re-create the upstream instead",
            ));
        }
        let new_alias = Self::validate_and_assign_alias(input.clone(), Some(&old_alias))?;
        self.validate_plugin_bindings(tenant_id, input.plugins.as_ref())
            .await?;
        self.check_shadow_allowed(ctx, tenant_id, &new_alias)
            .await?;
        let mut entity = input;
        entity.alias = Some(new_alias);
        entity.id = Some(gts::resource_instance_id(gts::UPSTREAM_TYPE_ID, &id));
        let record = UpstreamRecord { tenant_id, entity };
        self.repo
            .update_upstream(tenant_id, id, record.clone())
            .await?;
        Ok(record)
    }

    /// Reject an alias shadow of an ancestor upstream whose aligned
    /// capabilities are `private` (owner-only): a descendant may not create
    /// or keep a same-named entry that would hide it.
    ///
    /// Documented deviation (F-015 "alias shadowing"): an ancestor with
    /// `enforce`-shared capabilities is deliberately NOT rejected here.
    /// Descendant alias shadowing of an enforced ancestor is a supported
    /// tenant pattern — the ancestor's enforced auth/rate-limit/plugin/CORS
    /// scopes still apply to the descendant's upstream at the data-plane
    /// merge, and a descendant can never weaken an enforced ancestor policy,
    /// so there is nothing for a write-time `409` to protect. Only
    /// `private`-shared ancestors receive write-time shadow protection.
    ///
    /// # Errors
    ///
    /// Returns [`DomainError::Conflict`] when a `private`-shared ancestor owns
    /// the alias.
    async fn check_shadow_allowed(
        &self,
        ctx: &SecurityContext,
        tenant_id: Uuid,
        alias: &str,
    ) -> Result<(), DomainError> {
        let chain = self.tenant_chain(ctx, tenant_id).await?;
        let all = self.repo.list_upstreams_for_tenants(&chain).await?;
        for rec in all {
            if rec.tenant_id == tenant_id {
                continue;
            }
            let alias_matches = rec
                .entity
                .alias
                .as_deref()
                .is_some_and(|a| a.eq_ignore_ascii_case(alias));
            if alias_matches && rec.entity.has_private_sharing() {
                return Err(DomainError::Conflict {
                    detail: format!(
                        "alias {alias:?} is reserved by an ancestor tenant with \
                         private-sharing capabilities"
                    ),
                });
            }
        }
        Ok(())
    }

    /// Delete an upstream and the routes that referenced it.
    ///
    /// # Errors
    ///
    /// Returns [`DomainError::NotFound`] when the caller does not own it.
    pub async fn delete_upstream(&self, tenant_id: Uuid, id: Uuid) -> Result<(), DomainError> {
        // Compute the rate-scope keys the deleted resource's own / route
        // policies could have touched, before the rows are gone.
        let own_record = self.repo.get_upstream(tenant_id, id).await?;
        let routes = self.repo.list_routes_for_upstream(tenant_id, id).await?;
        let mut scope_keys: Vec<Option<String>> = Vec::new();
        let own_scope = own_record
            .entity
            .rate_limit
            .as_ref()
            .map_or(crate::domain::model::RateScope::Tenant, |rl| rl.scope);
        scope_keys.push(rate_scope_key(own_scope, tenant_id, ""));
        for route in &routes {
            let route_id = if let Some(opt) = Self::route_id_uuid(&route.entity) {
                opt.to_string()
            } else {
                String::new()
            };
            let scope = route
                .entity
                .rate_limit
                .as_ref()
                .map_or(crate::domain::model::RateScope::Tenant, |rl| rl.scope);
            scope_keys.push(rate_scope_key(scope, tenant_id, &route_id));
        }
        self.repo.delete_upstream(tenant_id, id).await?;
        self.evict_rate_keys_with(scope_keys);
        Ok(())
    }

    /// Fetch a caller-owned upstream.
    ///
    /// # Errors
    ///
    /// Returns [`DomainError::NotFound`] for ancestor/foreign resources.
    pub async fn get_upstream(
        &self,
        tenant_id: Uuid,
        id: Uuid,
    ) -> Result<UpstreamRecord, DomainError> {
        self.repo.get_upstream(tenant_id, id).await
    }

    /// List the caller's upstreams.
    ///
    /// # Errors
    ///
    /// Returns an internal error on repository failure.
    pub async fn list_upstreams(
        &self,
        tenant_id: Uuid,
    ) -> Result<Vec<UpstreamRecord>, DomainError> {
        self.repo.list_upstreams(tenant_id).await
    }

    /// Validate an (incoming or stored) upstream and derive/assign the
    /// effective alias. `existing_alias` is the current stored alias on
    /// updates.
    ///
    /// # Errors
    ///
    /// Returns [`DomainError::Validation`] with a human-readable reason.
    fn validate_and_assign_alias(
        input: Upstream,
        existing_alias: Option<&str>,
    ) -> Result<String, DomainError> {
        Self::validate_upstream_shape(&input)?;
        let derived = derive_alias(&input.server.endpoints);
        let effective = if let Some(d) = derived {
            // Derivable: a user alias must be absent or equal to the
            // derived value (idempotent no-op when exact).
            if let Some(provided) = &input.alias
                && !provided.eq_ignore_ascii_case(&d)
            {
                return Err(DomainError::validation(format!(
                    "alias for hostname endpoints is auto-derived (expected {d:?}, \
                     got {provided:?})"
                )));
            }
            d
        } else {
            let provided = input.alias.ok_or_else(|| {
                DomainError::validation(
                    "explicit alias is required for IP-based or non-derivable endpoints",
                )
            })?;
            normalize_alias(&provided).map_err(DomainError::validation)?
        };
        if let Some(existing) = existing_alias
            && !existing.eq_ignore_ascii_case(&effective)
        {
            return Err(DomainError::validation(
                "alias is immutable; delete and re-create the upstream instead",
            ));
        }
        Ok(effective)
    }

    /// Enforce the alias update-transition table (DESIGN §"Alias Update
    /// Behavior"): a derivable -> non-derivable transition is always
    /// rejected, even when the alias string itself is unchanged.
    ///
    /// # Errors
    ///
    /// Returns [`DomainError::Validation`] when the new payload is invalid.
    fn alias_transition_allowed(old: &Upstream, new: &Upstream) -> Result<bool, DomainError> {
        Self::validate_upstream_shape(new)?;
        let old_derivable = derive_alias(&old.server.endpoints).is_some();
        let new_derivable = derive_alias(&new.server.endpoints).is_some();
        Ok(!old_derivable || new_derivable)
    }

    /// Generic upstream shape validation (protocol, endpoints, pool
    /// homogeneity, tags, auth, rate limits, CORS).
    ///
    /// # Errors
    ///
    /// Returns [`DomainError::Validation`] with a human-readable reason.
    fn validate_upstream_shape(input: &Upstream) -> Result<(), DomainError> {
        if input.server.endpoints.is_empty() {
            return Err(DomainError::validation(
                "server.endpoints must not be empty",
            ));
        }
        if input.protocol != gts::PROTOCOL_HTTP_ID && input.protocol != gts::PROTOCOL_GRPC_ID {
            return Err(DomainError::validation(format!(
                "unsupported protocol {:?}",
                input.protocol
            )));
        }
        let mut scheme = None;
        let mut port = None;
        for ep in &input.server.endpoints {
            validate_host(&ep.host).map_err(DomainError::validation)?;
            if ep.port == Some(0) {
                return Err(DomainError::validation("endpoint port must be 1..=65535"));
            }
            let eff_port = ep.effective_port();
            if let Some(first_port) = port {
                if first_port != eff_port {
                    return Err(DomainError::validation(
                        "all endpoints in a pool must use the same port",
                    ));
                }
            } else {
                port = Some(eff_port);
            }
            let eff_scheme = ep.scheme.as_str();
            if let Some(first_scheme) = scheme {
                if first_scheme != eff_scheme {
                    return Err(DomainError::validation(
                        "all endpoints in a pool must use the same scheme",
                    ));
                }
            } else {
                scheme = Some(eff_scheme);
            }
            Self::validate_endpoint_scheme(input, ep)?;
        }
        for tag in &input.tags {
            if !Self::is_valid_tag(tag) {
                return Err(DomainError::validation(format!("invalid tag {tag:?}")));
            }
        }
        if let Some(auth) = &input.auth {
            if !auth.plugin_type.starts_with(gts::AUTH_PLUGIN_TYPE_ID) {
                return Err(DomainError::validation(format!(
                    "auth.type must be an auth plugin GTS id, got {:?}",
                    auth.plugin_type
                )));
            }
            if !KNOWN_AUTH_PLUGIN_IDS.contains(&auth.plugin_type.as_str()) {
                return Err(DomainError::validation(format!(
                    "auth.type {:?} is not a resolvable auth plugin id",
                    auth.plugin_type
                )));
            }
        }
        Self::validate_plugins_shape(input.plugins.as_ref())?;
        Self::validate_rate_limit(input.rate_limit.as_ref())?;
        Self::validate_cors(input.cors.as_ref())?;
        Self::validate_header_mutations(input.headers.as_ref())?;
        Ok(())
    }

    /// Store-time validation of header mutations (F-013b): every remove/set/
    /// add name and value must be representable as a real HTTP header so a
    /// stored mutation can never silently fail (and never reintroduce a
    /// non-existent header name) at the data plane.
    ///
    /// # Errors
    ///
    /// Returns [`DomainError::Validation`] naming the offending entry.
    fn validate_header_mutations(headers: Option<&HeadersConfig>) -> Result<(), DomainError> {
        let Some(headers) = headers else {
            return Ok(());
        };
        if let Some(r) = &headers.request {
            Self::validate_mutation_set("request", &r.set, &r.add, &r.remove, &r.passthrough_allowlist)?;
        }
        if let Some(r) = &headers.response {
            Self::validate_mutation_set("response", &r.set, &r.add, &r.remove, &[])?;
        }
        Ok(())
    }

    /// Check one `set`/`add`/`remove`/allowlist group for representability.
    ///
    /// # Errors
    ///
    /// Returns [`DomainError::Validation`] naming the offending entry.
    fn validate_mutation_set(
        phase: &str,
        set: &BTreeMap<String, String>,
        add: &BTreeMap<String, String>,
        remove: &[String],
        allowlist: &[String],
    ) -> Result<(), DomainError> {
        for (name, value) in set {
            if HeaderName::from_bytes(name.as_bytes()).is_err() {
                return Err(DomainError::validation(format!(
                    "{phase}.set header name {name:?} is not a valid HTTP header name"
                )));
            }
            if HeaderValue::from_str(value).is_err() {
                return Err(DomainError::validation(format!(
                    "{phase}.set header value {value:?} is not a valid HTTP header value"
                )));
            }
        }
        for (name, value) in add {
            if HeaderName::from_bytes(name.as_bytes()).is_err() {
                return Err(DomainError::validation(format!(
                    "{phase}.add header name {name:?} is not a valid HTTP header name"
                )));
            }
            if HeaderValue::from_str(value).is_err() {
                return Err(DomainError::validation(format!(
                    "{phase}.add header value {value:?} is not a valid HTTP header value"
                )));
            }
        }
        for name in remove.iter().chain(allowlist) {
            if HeaderName::from_bytes(name.as_bytes()).is_err() {
                return Err(DomainError::validation(format!(
                    "{phase} header name {name:?} is not a valid HTTP header name"
                )));
            }
        }
        Ok(())
    }

    /// Syntactic validation of a plugin chain (non-empty references). Actual
    /// resolvability is checked separately with repository access.
    fn validate_plugins_shape(plugins: Option<&PluginsBinding>) -> Result<(), DomainError> {
        let Some(plugins) = plugins else {
            return Ok(());
        };
        for item in &plugins.items {
            let (reference, _) = item.as_ref();
            if reference.trim().is_empty() {
                return Err(DomainError::validation(
                    "plugin chain entries must not be empty",
                ));
            }
        }
        Ok(())
    }

    /// Resolve the plugin chain at write time: every entry must name a known
    /// built-in / catalog GTS id, or a custom plugin the caller owns.
    ///
    /// # Errors
    ///
    /// Returns [`DomainError::Validation`] for an unresolvable reference.
    async fn validate_plugin_bindings(
        &self,
        tenant_id: Uuid,
        plugins: Option<&PluginsBinding>,
    ) -> Result<(), DomainError> {
        let Some(plugins) = plugins else {
            return Ok(());
        };
        for item in &plugins.items {
            let (reference, _) = item.as_ref();
            if KNOWN_PLUGIN_IDS.contains(&reference) {
                continue;
            }
            match gts::parse_resource_id(reference) {
                Some(custom_id) => {
                    self.repo.get_plugin(tenant_id, custom_id).await?;
                }
                None => {
                    return Err(DomainError::validation(format!(
                        "plugin reference {reference:?} is not a known plugin id"
                    )));
                }
            }
        }
        Ok(())
    }

    fn validate_endpoint_scheme(input: &Upstream, ep: &Endpoint) -> Result<(), DomainError> {
        let scheme = ep.scheme.as_str();
        let ok = if input.is_grpc() {
            scheme == "grpc"
        } else {
            matches!(scheme, "https" | "wss")
        };
        if ok {
            Ok(())
        } else {
            Err(DomainError::validation(format!(
                "scheme {scheme:?} is not valid for protocol {:?}",
                input.protocol
            )))
        }
    }

    fn is_valid_tag(tag: &str) -> bool {
        !tag.is_empty()
            && tag.len() <= 64
            && tag
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
    }

    fn validate_rate_limit(rl: Option<&RateLimit>) -> Result<(), DomainError> {
        let Some(rl) = rl else {
            return Ok(());
        };
        if rl.sustained.rate == 0 {
            return Err(DomainError::validation(
                "rate_limit.sustained.rate must be >= 1",
            ));
        }
        if rl.capacity() == 0 {
            return Err(DomainError::validation(
                "rate_limit burst capacity must be >= 1",
            ));
        }
        if rl.cost == 0 {
            return Err(DomainError::validation("rate_limit.cost must be >= 1"));
        }
        // A per-request cost above the bucket capacity can never be served.
        if rl.cost > rl.capacity() {
            return Err(DomainError::validation(
                "rate_limit.cost must not exceed the burst capacity",
            ));
        }
        Ok(())
    }

    /// Validate a CORS origin by actually parsing it (a bare
    /// `foo.example` or a malformed origin is rejected, not just a missing
    /// scheme prefix).
    fn validate_cors(cors: Option<&CorsConfig>) -> Result<(), DomainError> {
        let Some(cors) = cors else {
            return Ok(());
        };
        if !cors.enabled {
            return Ok(());
        }
        if cors.allow_credentials && cors.allowed_origins.iter().any(|o| o == "*") {
            return Err(DomainError::validation(
                "allow_credentials cannot be combined with wildcard origin '*'",
            ));
        }
        for origin in &cors.allowed_origins {
            if origin == "*" {
                continue;
            }
            let parsed = Url::parse(origin).map_err(|e| {
                DomainError::validation(format!("invalid CORS origin {origin:?}: {e}"))
            })?;
            if !matches!(parsed.scheme(), "https" | "http") {
                return Err(DomainError::validation(format!(
                    "invalid CORS origin {origin:?}: scheme must be https or http"
                )));
            }
        }
        Ok(())
    }

    // =======================================================================
    // Route CRUD
    // =======================================================================

    /// Create a route belonging to a caller-owned upstream.
    ///
    /// # Errors
    ///
    /// Returns validation, not-found or conflict errors.
    pub async fn create_route(
        &self,
        tenant_id: Uuid,
        input: Route,
    ) -> Result<RouteRecord, DomainError> {
        // Normalize the binding to a bare UUID so stored ids are canonical
        // regardless of the wire form (GTS id or UUID).
        let upstream_uuid = gts::parse_resource_id(&input.upstream_id)
            .ok_or_else(|| DomainError::validation("upstream_id must be a UUID"))?;
        // Ancestor upstreams are not addressable through the management API.
        self.repo.get_upstream(tenant_id, upstream_uuid).await?;
        Self::validate_route_shape(&input)?;
        Self::validate_plugins_shape(input.plugins.as_ref())?;
        self.validate_plugin_bindings(tenant_id, input.plugins.as_ref())
            .await?;
        self.check_route_match_unique(tenant_id, upstream_uuid, None, &input)
            .await?;
        let id = Uuid::new_v4();
        let mut entity = input;
        entity.upstream_id = upstream_uuid.to_string();
        entity.id = Some(gts::resource_instance_id(gts::ROUTE_TYPE_ID, &id));
        let record = RouteRecord { tenant_id, entity };
        self.repo.insert_route(record.clone()).await?;
        Ok(record)
    }

    /// Replace a route (upstream binding is immutable).
    ///
    /// # Errors
    ///
    /// Returns [`DomainError::NotFound`] when the caller does not own it.
    pub async fn update_route(
        &self,
        tenant_id: Uuid,
        id: Uuid,
        input: Route,
    ) -> Result<RouteRecord, DomainError> {
        let existing = self.repo.get_route(tenant_id, id).await?;
        let incoming_uuid = gts::parse_resource_id(&input.upstream_id)
            .ok_or_else(|| DomainError::validation("upstream_id must be a UUID"))?;
        if !existing
            .entity
            .upstream_id
            .eq_ignore_ascii_case(&incoming_uuid.to_string())
        {
            return Err(DomainError::validation("route.upstream_id is immutable"));
        }
        self.repo.get_upstream(tenant_id, incoming_uuid).await?;
        Self::validate_route_shape(&input)?;
        Self::validate_plugins_shape(input.plugins.as_ref())?;
        self.validate_plugin_bindings(tenant_id, input.plugins.as_ref())
            .await?;
        self.check_route_match_unique(tenant_id, incoming_uuid, Some(id), &input)
            .await?;
        let mut entity = input;
        entity.upstream_id = incoming_uuid.to_string();
        entity.id = Some(gts::resource_instance_id(gts::ROUTE_TYPE_ID, &id));
        let record = RouteRecord { tenant_id, entity };
        self.repo.update_route(tenant_id, record.clone()).await?;
        Ok(record)
    }

    /// Delete a route.
    ///
    /// # Errors
    ///
    /// Returns [`DomainError::NotFound`] when the caller does not own it.
    pub async fn delete_route(&self, tenant_id: Uuid, id: Uuid) -> Result<(), DomainError> {
        let record = self.repo.get_route(tenant_id, id).await?;
        let route_id = Self::route_id_uuid(&record.entity).unwrap_or_default();
        let scope = record
            .entity
            .rate_limit
            .as_ref()
            .map_or(crate::domain::model::RateScope::Tenant, |rl| rl.scope);
        self.repo.delete_route(tenant_id, id).await?;
        self.evict_rate_keys_with([rate_scope_key(scope, tenant_id, &route_id.to_string())]);
        Ok(())
    }

    /// Fetch a caller-owned route.
    ///
    /// # Errors
    ///
    /// Returns [`DomainError::NotFound`] for foreign/ancestor resources.
    pub async fn get_route(&self, tenant_id: Uuid, id: Uuid) -> Result<RouteRecord, DomainError> {
        self.repo.get_route(tenant_id, id).await
    }

    /// List the caller's routes.
    ///
    /// # Errors
    ///
    /// Returns an internal error on repository failure.
    pub async fn list_routes(&self, tenant_id: Uuid) -> Result<Vec<RouteRecord>, DomainError> {
        self.repo.list_routes(tenant_id).await
    }

    fn validate_route_shape(route: &Route) -> Result<(), DomainError> {
        match (&route.match_rule.http, &route.match_rule.grpc) {
            (Some(_), Some(_)) | (None, None) => {
                return Err(DomainError::validation(
                    "match must contain exactly one of http|grpc",
                ));
            }
            (Some(http), None) => Self::validate_http_match(http)?,
            (None, Some(grpc)) => {
                if grpc.service.is_empty() || grpc.method.is_empty() {
                    return Err(DomainError::validation(
                        "grpc match requires service and method",
                    ));
                }
            }
        }
        Self::validate_rate_limit(route.rate_limit.as_ref())?;
        Self::validate_cors(route.cors.as_ref())?;
        for tag in &route.tags {
            if !Self::is_valid_tag(tag) {
                return Err(DomainError::validation(format!("invalid tag {tag:?}")));
            }
        }
        Ok(())
    }

    fn validate_http_match(http: &HttpMatch) -> Result<(), DomainError> {
        if http.methods.is_empty() {
            return Err(DomainError::validation(
                "match.http.methods must not be empty",
            ));
        }
        if http.path.is_empty() {
            return Err(DomainError::validation("match.http.path must not be empty"));
        }
        if !http.path.starts_with('/') {
            return Err(DomainError::validation(
                "match.http.path must start with '/'",
            ));
        }
        Ok(())
    }

    /// Enforce route match-rule uniqueness within an upstream: same path and
    /// overlapping methods is a conflict. `skip_id` excludes the route being
    /// replaced.
    #[allow(clippy::too_many_lines)]
    async fn check_route_match_unique(
        &self,
        tenant_id: Uuid,
        upstream_uuid: Uuid,
        skip_id: Option<Uuid>,
        incoming: &Route,
    ) -> Result<(), DomainError> {
        let existing = self
            .repo
            .list_routes_for_upstream(tenant_id, upstream_uuid)
            .await?;
        let Some(incoming_http) = &incoming.match_rule.http else {
            // gRPC routes: only one per (service, method) today.
            let grpc = incoming.match_rule.grpc.as_ref().ok_or_else(|| {
                DomainError::validation("match must contain exactly one of http|grpc")
            })?;
            for rec in existing {
                if Self::route_id_uuid(&rec.entity) == skip_id {
                    continue;
                }
                if let Some(other) = &rec.entity.match_rule.grpc
                    && other.service == grpc.service
                    && other.method == grpc.method
                {
                    return Err(DomainError::Conflict {
                        detail: format!(
                            "route for {}::{} already exists",
                            grpc.service, grpc.method
                        ),
                    });
                }
            }
            return Ok(());
        };
        let incoming_path = incoming_http.path.trim_end_matches('/');
        for rec in existing {
            if Self::route_id_uuid(&rec.entity) == skip_id {
                continue;
            }
            let Some(other_http) = &rec.entity.match_rule.http else {
                continue;
            };
            if other_http.path.trim_end_matches('/') != incoming_path {
                continue;
            }
            // Distinct explicit priorities allow same-path routes (a shadow /
            // fallback shape); equal priorities must not overlap methods.
            if rec.entity.priority != incoming.priority {
                continue;
            }
            let overlap = other_http
                .methods
                .iter()
                .any(|m| incoming_http.methods.iter().any(|im| im == m));
            if overlap {
                return Err(DomainError::Conflict {
                    detail: format!(
                        "a route for path {:?} at priority {} with overlapping methods \
                         already exists",
                        incoming_http.path, incoming.priority
                    ),
                });
            }
        }
        Ok(())
    }

    fn route_id_uuid(route: &Route) -> Option<Uuid> {
        route.id.as_deref().and_then(gts::parse_resource_id)
    }

    // =======================================================================
    // Plugin CRUD
    // =======================================================================

    /// Create a custom (Starlark) plugin.
    ///
    /// # Errors
    ///
    /// Returns [`DomainError::Validation`] when the payload is malformed.
    pub async fn create_plugin(
        &self,
        tenant_id: Uuid,
        input: Plugin,
    ) -> Result<PluginRecord, DomainError> {
        if input.name.trim().is_empty() {
            return Err(DomainError::validation("plugin name must not be empty"));
        }
        if input.source_code.trim().is_empty() {
            return Err(DomainError::validation(
                "plugin source_code must not be empty",
            ));
        }
        if !matches!(input.config_schema, serde_json::Value::Object(_)) {
            return Err(DomainError::validation(
                "plugin config_schema must be a JSON object (draft-07 schema)",
            ));
        }
        let id = Uuid::new_v4();
        let mut entity = input;
        entity.id = Some(gts::resource_instance_id(entity.kind.type_id(), &id));
        let record = PluginRecord { tenant_id, entity };
        self.repo.insert_plugin(record.clone()).await?;
        Ok(record)
    }

    /// Delete a custom plugin, refusing while it is still referenced.
    ///
    /// # Errors
    ///
    /// Returns [`DomainError::NotFound`] or [`DomainError::PluginInUse`].
    pub async fn delete_plugin(&self, tenant_id: Uuid, id: Uuid) -> Result<(), DomainError> {
        let record = self.repo.get_plugin(tenant_id, id).await?;
        let key = record.entity.id.clone().unwrap_or_else(|| id.to_string());
        // Refuse before touching the store so a referenced plugin is never
        // destroyed behind the 409.
        if self.repo.plugin_is_referenced(tenant_id, &key).await? {
            return Err(DomainError::PluginInUse);
        }
        self.repo.delete_plugin(tenant_id, id).await
    }

    /// Fetch a caller-owned custom plugin.
    ///
    /// # Errors
    ///
    /// Returns [`DomainError::NotFound`] for foreign/ancestor resources.
    pub async fn get_plugin(&self, tenant_id: Uuid, id: Uuid) -> Result<PluginRecord, DomainError> {
        self.repo.get_plugin(tenant_id, id).await
    }

    /// List the caller's custom plugins.
    ///
    /// # Errors
    ///
    /// Returns an internal error on repository failure.
    pub async fn list_plugins(&self, tenant_id: Uuid) -> Result<Vec<PluginRecord>, DomainError> {
        self.repo.list_plugins(tenant_id).await
    }
}
