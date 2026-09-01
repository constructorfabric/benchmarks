//! Control Plane implementation.
//!
//! Implements [`ControlPlaneService`] over the in-memory repositories, the
//! alias-derivation matrix, the tenant-chain walk (via the `tenant-resolver`
//! client published in the `ClientHub`), the permission checks of
//! `DESIGN.md` §3.2 (via the `authz-resolver` client) and the hierarchical
//! merge.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use authz_resolver_sdk::{
    Action, AuthZResolverClient, EvaluationRequest, EvaluationRequestContext, Resource, Subject,
    TenantContext,
};
use tenant_resolver_sdk::{TenantId, TenantResolverClient};
use toolkit_security::SecurityContext;

use crate::domain::alias;
use crate::domain::error::DomainError;
use crate::domain::gts;
use crate::domain::model::{
    AuthConfig, CorsConfig, HeadersConfig, Plugin, PluginType, PluginsConfig, RateLimitConfig,
    Route, Upstream,
};
use crate::domain::plugin::{PluginChain, PluginInvocation};
use crate::domain::routing::RouteCandidate;
use crate::domain::services::management::{
    AncestorSnapshot, ControlPlaneService, ListQuery, PluginSource, ResolvedTarget,
};
use crate::infra::storage::InMemoryStore;

/// Maximum depth of the tenant hierarchy walk (guards against cycles).
const MAX_TENANT_DEPTH: usize = 32;

/// GTS base type of the proxy resource (`DESIGN.md` §3.2 proxy permissions).
const PROXY_TYPE_ID: &str = "gts.cf.core.oagw.proxy.v1~";

/// `Retry-After` hint emitted with a 503 `upstream.disabled` problem.
pub const UPSTREAM_DISABLED_RETRY_AFTER_SECS: u64 = 30;

/// The concrete Control Plane service.
pub struct ControlPlaneServiceImpl {
    store: InMemoryStore,
    tenants: Option<Arc<dyn TenantResolverClient>>,
    authz: Option<Arc<dyn AuthZResolverClient>>,
    allow_http_upstream: bool,
    /// Set once the absent-`authz` warning has been emitted, so the fail-open
    /// posture is logged a single time instead of per request.
    warned_missing_authz: AtomicBool,
}

impl std::fmt::Debug for ControlPlaneServiceImpl {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ControlPlaneServiceImpl")
            .field("upstreams", &self.store.upstream_count())
            .field("routes", &self.store.route_count())
            .field("plugins", &self.store.plugin_count())
            .finish()
    }
}

/// A parsed OData `$filter` term: `(field, value)`.
type FilterTerm = (String, String);

/// The outcome of resolving an alias along the tenant chain.
#[derive(Debug)]
enum AliasSelection {
    /// The alias resolves to the closest enabled upstream. Boxed because
    /// [`Upstream`] is large compared with the two data-free variants.
    Resolved(Box<Upstream>),
    /// An upstream with this alias is disabled somewhere on the chain: in the
    /// caller's tenant (the closest match wins, and it is disabled) or in an
    /// ancestor (an ancestor-disabled alias is disabled for every descendant).
    Disabled,
    /// No upstream with this alias anywhere on the chain.
    Absent,
}

/// The management and data-plane permissions of `DESIGN.md` §3.2.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Permission {
    /// `POST` on a resource.
    Create,
    /// Full replacement of a resource.
    Override,
    /// `GET` on a resource.
    Read,
    /// `DELETE` on a resource.
    Delete,
    /// Binding to an ancestor upstream's alias.
    Bind,
    /// Proxying a request to an upstream.
    Invoke,
}

impl Permission {
    /// The action name sent to the PDP, per `DESIGN.md` §3.2.
    #[must_use]
    fn action(self) -> &'static str {
        match self {
            Self::Create => "create",
            Self::Override => "override",
            Self::Read => "read",
            Self::Delete => "delete",
            Self::Bind => "bind",
            Self::Invoke => "invoke",
        }
    }
}

impl ControlPlaneServiceImpl {
    /// Builds the Control Plane around an in-memory store.
    ///
    /// `tenants` is optional: when no `tenant-resolver` client is published in
    /// the `ClientHub`, the tenant chain degenerates to the calling tenant
    /// alone and hierarchical shadowing is disabled.
    #[must_use]
    pub fn new(store: InMemoryStore, tenants: Option<Arc<dyn TenantResolverClient>>) -> Self {
        Self {
            store,
            tenants,
            authz: None,
            allow_http_upstream: false,
            warned_missing_authz: AtomicBool::new(false),
        }
    }

    /// Publishes the `authz-resolver` client used for permission checks.
    ///
    /// Without it (`DESIGN.md` §3.2 requires the `authz_resolver` gear) every
    /// permission check fails open and the omission is logged once, so the
    /// in-memory test harness keeps working.
    #[must_use]
    pub fn with_authz(mut self, authz: Option<Arc<dyn AuthZResolverClient>>) -> Self {
        self.authz = authz;
        self
    }

    /// Permits cleartext `http` endpoints in this deployment.
    ///
    /// Mirrors `allow_http_upstream` of `OagwConfig`: when it is `false` (the
    /// default) a cleartext endpoint is rejected at creation time, so a
    /// misconfiguration surfaces as a 400 instead of a per-request 502.
    #[must_use]
    pub fn allowing_http_upstream(mut self, allow: bool) -> Self {
        self.allow_http_upstream = allow;
        self
    }

    /// The backing store (diagnostics and tests).
    #[must_use]
    pub fn store(&self) -> &InMemoryStore {
        &self.store
    }

    /// Parses a resource identifier that may be a bare UUID or a GTS
    /// `~<instance>` tail.
    fn parse_id(id: &str) -> Option<uuid::Uuid> {
        gts::uuid_from_instance_id(id).or_else(|| uuid::Uuid::parse_str(id).ok())
    }

    /// The calling tenant, or a validation error when the context is anonymous.
    fn caller_tenant(ctx: &SecurityContext) -> Result<uuid::Uuid, DomainError> {
        let tenant = ctx.subject_tenant_id();
        if tenant.is_nil() {
            return Err(DomainError::Validation(
                "the request carries no tenant identity".to_owned(),
            ));
        }
        Ok(tenant)
    }

    /// The tenant chain of the caller, root first, caller last.
    ///
    /// # Errors
    ///
    /// 503 [`DomainError::TenantResolution`] when the hierarchy cannot be
    /// resolved. Ancestor `enforce` constraints are safety-critical, so a
    /// resolver failure is never swallowed into a truncated chain: a truncated
    /// chain would silently drop every ancestor constraint.
    async fn tenant_chain(&self, ctx: &SecurityContext) -> Result<Vec<uuid::Uuid>, DomainError> {
        let Some(client) = self.tenants.as_ref() else {
            return Ok(vec![ctx.subject_tenant_id()]);
        };
        let mut chain: Vec<uuid::Uuid> = Vec::new();
        let mut current = ctx.subject_tenant_id();
        for _ in 0..MAX_TENANT_DEPTH {
            let info = match client.get_tenant(ctx, TenantId(current)).await {
                Ok(info) => info,
                Err(error) => {
                    tracing::error!(
                        tenant_id = %current,
                        %error,
                        "tenant hierarchy could not be resolved; refusing to continue with a truncated chain"
                    );
                    return Err(DomainError::TenantResolution(format!(
                        "the tenant hierarchy of '{current}' could not be resolved: {error}"
                    )));
                }
            };
            let next = info.parent_id.filter(|parent| !parent.is_nil());
            chain.push(info.id.0);
            match next {
                Some(parent) => current = parent.0,
                None => break,
            }
        }
        chain.retain(|id| !id.is_nil());
        chain.reverse();
        if chain.is_empty() {
            return Err(DomainError::TenantResolution(format!(
                "the tenant hierarchy of '{current}' is empty"
            )));
        }
        Ok(chain)
    }

    /// Enforces a `DESIGN.md` §3.2 permission on a resource type.
    ///
    /// Fails **open** when no `authz-resolver` client is published (the
    /// in-memory harness and single-gear deployments) and when the PDP is
    /// unreachable: the gateway stays available and the omission is logged. An
    /// explicit PDP denial always fails closed.
    ///
    /// # Errors
    ///
    /// 403 [`DomainError::Forbidden`] when the PDP denies the action.
    async fn ensure_permission(
        &self,
        ctx: &SecurityContext,
        resource_type: &str,
        permission: Permission,
    ) -> Result<(), DomainError> {
        let Some(authz) = self.authz.as_ref() else {
            if !self.warned_missing_authz.swap(true, Ordering::Relaxed) {
                tracing::warn!(
                    resource_type,
                    action = permission.action(),
                    "no authz-resolver client is published: permission checks fail open"
                );
            }
            return Ok(());
        };
        let tenant = ctx.subject_tenant_id();
        let mut properties = std::collections::HashMap::new();
        properties.insert("tenant_id".to_owned(), serde_json::json!(tenant));
        let request = EvaluationRequest {
            subject: Subject {
                id: ctx.subject_id(),
                subject_type: ctx.subject_type().map(str::to_owned),
                properties,
            },
            action: Action {
                name: permission.action().to_owned(),
            },
            resource: Resource {
                resource_type: resource_type.to_owned(),
                id: None,
                properties: std::collections::HashMap::new(),
            },
            context: EvaluationRequestContext {
                tenant_context: Some(TenantContext {
                    root_id: Some(tenant),
                    ..TenantContext::default()
                }),
                token_scopes: ctx.token_scopes().to_vec(),
                require_constraints: false,
                capabilities: Vec::new(),
                supported_properties: Vec::new(),
                bearer_token: ctx.bearer_token().cloned(),
            },
        };
        match authz.evaluate(request).await {
            Ok(response) if response.decision => Ok(()),
            Ok(response) => {
                let reason = response
                    .context
                    .deny_reason
                    .as_ref()
                    .map(|reason| {
                        reason
                            .details
                            .clone()
                            .unwrap_or_else(|| reason.error_code.clone())
                    })
                    .unwrap_or_else(|| "denied by policy".to_owned());
                Err(DomainError::Forbidden(format!(
                    "{} on {} is not permitted: {reason}",
                    permission.action(),
                    resource_type
                )))
            }
            Err(error) => {
                tracing::warn!(
                    resource_type,
                    action = permission.action(),
                    %error,
                    "authz-resolver evaluation failed: permission checks fail open"
                );
                Ok(())
            }
        }
    }

    /// Resolves an alias along the tenant chain, closest tenant first.
    ///
    /// A disabled upstream always wins the lookup over an enabled one: a
    /// disabled upstream rejects every proxy request with 503, and an
    /// ancestor-disabled alias is disabled for every descendant, which cannot
    /// re-enable it (`PRD.md` §5.1 "Enable/Disable Semantics").
    fn resolve_alias(&self, chain: &[uuid::Uuid], normalized_alias: &str) -> AliasSelection {
        let mut closest: Option<Box<Upstream>> = None;
        let mut disabled = false;
        for tenant_id in chain.iter().rev() {
            let Some(upstream) = self.store.upstream_by_alias(*tenant_id, normalized_alias) else {
                continue;
            };
            if !upstream.enabled {
                disabled = true;
            } else if closest.is_none() {
                closest = Some(Box::new(upstream));
            }
        }
        if disabled {
            return AliasSelection::Disabled;
        }
        closest.map_or(AliasSelection::Absent, AliasSelection::Resolved)
    }

    /// Ancestor snapshots of the selected upstream, root first.
    ///
    /// Disabled ancestors are skipped: they contribute neither their `enforce`
    /// rate limit nor their auth, plugin or CORS configuration, and they cannot
    /// shadow the selected upstream.
    fn ancestor_snapshots(
        &self,
        chain: &[uuid::Uuid],
        normalized_alias: &str,
        selected_tenant: uuid::Uuid,
    ) -> Vec<AncestorSnapshot> {
        let Some(position) = chain.iter().position(|tenant| *tenant == selected_tenant) else {
            return Vec::new();
        };
        let mut snapshots: Vec<AncestorSnapshot> = Vec::new();
        for tenant in &chain[..position] {
            if let Some(upstream) = self.store.upstream_by_alias(*tenant, normalized_alias)
                && upstream.enabled
            {
                snapshots.push(AncestorSnapshot {
                    tenant_id: *tenant,
                    upstream,
                });
            }
        }
        snapshots
    }

    fn validate_upstream(
        upstream: &Upstream,
        allow_http_upstream: bool,
    ) -> Result<(), DomainError> {
        if upstream.protocol.is_empty() {
            return Err(DomainError::Validation(
                "protocol is required (a GTS identifier)".to_owned(),
            ));
        }
        if !allow_http_upstream
            && upstream
                .server
                .endpoints
                .iter()
                .any(|endpoint| endpoint.scheme == crate::domain::model::EndpointScheme::Http)
        {
            return Err(DomainError::Validation(
                "cleartext http endpoints require allow_http_upstream = true".to_owned(),
            ));
        }
        alias::validate_endpoints(&upstream.server.endpoints)?;
        if let Some(cors) = &upstream.cors {
            crate::domain::cors::validate_config(cors)?;
        }
        validate_plugin_bindings(upstream.plugins.as_ref())?;
        if let Some(auth) = &upstream.auth
            && let Some(reference) = auth.plugin_ref()
        {
            validate_auth_slot(reference)?;
        }
        Ok(())
    }

    fn validate_route(route: &Route) -> Result<(), DomainError> {
        if let Some(cors) = &route.cors {
            crate::domain::cors::validate_config(cors)?;
        }
        validate_plugin_bindings(route.plugins.as_ref())?;
        if let Some(http) = route.match_config.http.as_ref() {
            if http.methods.is_empty() {
                return Err(DomainError::Validation(
                    "route match requires at least one method".to_owned(),
                ));
            }
            if !http.path.starts_with('/') {
                return Err(DomainError::Validation(
                    "route match path must start with '/'".to_owned(),
                ));
            }
            return Ok(());
        }
        // gRPC matching is catalogued only (DESIGN §4.7).
        route.match_config.grpc.as_ref().map_or_else(
            || {
                Err(DomainError::Validation(
                    "route match requires either an http or a grpc block".to_owned(),
                ))
            },
            |_| Ok(()),
        )
    }

    /// Validates a bind to an ancestor upstream that already owns the alias.
    ///
    /// `DESIGN.md` §3.1: "If alias matches an ancestor upstream, the operation
    /// is a 'bind' requiring `oagw:upstream:bind` permission and respecting
    /// sharing mode constraints (`enforce` blocks overrides, `private` blocks
    /// visibility)".
    ///
    /// # Errors
    ///
    /// 403 when the closest ancestor upstream carrying the alias enforces its
    /// `auth`, `plugins` or `rate_limit` configuration, or when the caller
    /// lacks the `oagw:upstream:bind` permission.
    async fn ensure_ancestor_bind_allowed(
        &self,
        ctx: &SecurityContext,
        chain: &[uuid::Uuid],
        upstream: &Upstream,
    ) -> Result<(), DomainError> {
        let Some(ancestor) = chain
            .iter()
            .filter(|tenant| **tenant != upstream.tenant_id)
            .rev()
            .find_map(|tenant| {
                self.store
                    .upstream_by_alias(*tenant, &upstream.alias)
                    .filter(|ancestor| ancestor.enabled)
            })
        else {
            return Ok(());
        };
        let enforced = [
            ancestor.plugins.as_ref().map(|plugins| plugins.sharing),
            ancestor.auth.as_ref().map(|auth| auth.sharing),
            ancestor.rate_limit.as_ref().map(|limit| limit.sharing),
        ]
        .into_iter()
        .flatten()
        .any(|sharing| sharing == crate::domain::model::SharingMode::Enforce);
        if enforced {
            return Err(DomainError::Forbidden(format!(
                "alias '{}' is owned by an ancestor upstream that enforces its configuration and cannot be overridden",
                upstream.alias
            )));
        }
        self.ensure_permission(ctx, gts::UPSTREAM_TYPE_ID, Permission::Bind)
            .await
    }

    /// Rejects a second enabled route with the same `(upstream, match)`.
    ///
    /// # Errors
    ///
    /// 409 [`DomainError::Conflict`] when another route already occupies the
    /// match key.
    fn ensure_unique_match(
        &self,
        upstream_id: uuid::Uuid,
        route: &Route,
        excluding: Option<uuid::Uuid>,
    ) -> Result<(), DomainError> {
        for existing in self.store.routes_of_upstream(upstream_id) {
            if Some(existing.id) == excluding {
                continue;
            }
            if existing.enabled && existing.match_key() == route.match_key() {
                return Err(DomainError::Conflict(format!(
                    "route {} already matches {}",
                    existing.id,
                    route.match_key()
                )));
            }
        }
        Ok(())
    }

    fn ensure_unique_alias(
        &self,
        tenant_id: uuid::Uuid,
        upstream: &Upstream,
        excluding: Option<uuid::Uuid>,
    ) -> Result<(), DomainError> {
        if let Some(existing) = self.store.upstream_by_alias(tenant_id, &upstream.alias)
            && Some(existing.id) != excluding
        {
            return Err(DomainError::Conflict(format!(
                "an upstream with alias '{}' already exists in this tenant",
                upstream.alias
            )));
        }
        Ok(())
    }

    fn ensure_unique_plugin_name(
        &self,
        tenant_id: uuid::Uuid,
        plugin: &Plugin,
        excluding: Option<uuid::Uuid>,
    ) -> Result<(), DomainError> {
        if let Some(existing) = self.store.plugin_by_name(tenant_id, &plugin.name)
            && Some(existing.id) != excluding
        {
            return Err(DomainError::Conflict(format!(
                "a plugin named '{}' already exists in this tenant",
                plugin.name
            )));
        }
        Ok(())
    }

    fn plugin_references(&self, plugin_id: uuid::Uuid) -> (Vec<String>, Vec<String>) {
        let tail = plugin_id.to_string();
        let mut upstreams: Vec<String> = Vec::new();
        let mut routes: Vec<String> = Vec::new();
        for upstream in self.store.all_upstreams() {
            let Some(plugins) = &upstream.plugins else {
                continue;
            };
            if plugins
                .items
                .iter()
                .any(|b| b.plugin_ref().ends_with(&tail))
            {
                upstreams.push(upstream.alias);
            }
        }
        for route in self.store.all_routes() {
            if let Some(plugins) = &route.plugins
                && plugins
                    .items
                    .iter()
                    .any(|b| b.plugin_ref().ends_with(&tail))
            {
                routes.push(route.id.to_string());
            }
        }
        (upstreams, routes)
    }

    fn alias_is_common_suffix(upstream: &Upstream) -> bool {
        upstream.server.endpoints.len() > 1
            && alias::compute_derived_alias(&upstream.server.endpoints)
                .is_some_and(|derived| derived == upstream.alias)
    }

    /// Parses an OData `$filter` expression into `and`-joined `eq` terms.
    ///
    /// # Errors
    ///
    /// 400 [`DomainError::Validation`] for an unsupported expression.
    fn parse_filter(query: &ListQuery) -> Result<Option<Vec<FilterTerm>>, DomainError> {
        let Some(expression) = query.filter.as_deref() else {
            return Ok(None);
        };
        let expression = expression.trim();
        if expression.is_empty() {
            return Ok(None);
        }
        let mut terms: Vec<FilterTerm> = Vec::new();
        for term in expression.split(" and ") {
            let term = term.trim();
            let Some((field, rest)) = term.split_once(" eq ") else {
                return Err(DomainError::Validation(format!(
                    "unsupported $filter expression '{term}'"
                )));
            };
            terms.push((
                field.trim().to_owned(),
                rest.trim().trim_matches('\'').to_owned(),
            ));
        }
        Ok(Some(terms))
    }

    /// Applies `$filter`, `$orderby`, `$skip` and `$top` to a list.
    ///
    /// `$select` names the projected fields, which the transport layer must
    /// apply when it serializes the DTOs: [`ListQuery`] carries the names but
    /// this method returns whole resources.
    fn list_filtered<T, F>(
        &self,
        items: Vec<T>,
        query: &ListQuery,
        field: F,
    ) -> Result<Vec<T>, DomainError>
    where
        F: Fn(&T, &str) -> Option<String>,
    {
        let mut items = items;
        if let Some(terms) = Self::parse_filter(query)? {
            items.retain(|item| {
                terms
                    .iter()
                    .all(|(name, value)| field(item, name).as_deref() == Some(value.as_str()))
            });
        }
        if let Some(expression) = query.orderby.as_deref() {
            let expression = expression.trim();
            let (name, descending) = match expression.split_once(' ') {
                Some((name, direction)) if direction.eq_ignore_ascii_case("desc") => {
                    (name.trim(), true)
                }
                Some((name, _)) => (name.trim(), false),
                None => (expression, false),
            };
            if descending {
                items.sort_by(|left, right| field(left, name).cmp(&field(right, name)).reverse());
            } else {
                items.sort_by_key(|item| field(item, name));
            }
        }
        let skip = usize::try_from(query.skip.unwrap_or(0)).unwrap_or(usize::MAX);
        if skip > 0 {
            items.drain(..skip.min(items.len()));
        }
        let top = usize::try_from(query.effective_top()).unwrap_or(usize::MAX);
        items.truncate(top);
        Ok(items)
    }
}

fn upstream_field(upstream: &Upstream, name: &str) -> Option<String> {
    match name {
        "id" | "id_" => Some(upstream.id.to_string()),
        "alias" => Some(upstream.alias.clone()),
        "protocol" => Some(upstream.protocol.clone()),
        "enabled" => Some(upstream.enabled.to_string()),
        "tenant_id" => Some(upstream.tenant_id.to_string()),
        _ => None,
    }
}

fn route_field(route: &Route, name: &str) -> Option<String> {
    match name {
        "id" => Some(route.id.to_string()),
        "upstream_id" => Some(route.upstream_id.to_string()),
        "enabled" => Some(route.enabled.to_string()),
        "priority" => Some(route.priority.to_string()),
        "path" => route
            .match_config
            .http
            .as_ref()
            .map(|http| http.path.clone()),
        _ => None,
    }
}

fn plugin_field(plugin: &Plugin, name: &str) -> Option<String> {
    match name {
        "id" => Some(plugin.id.to_string()),
        "name" => Some(plugin.name.clone()),
        "plugin_type" | "type" => Some(
            match plugin.plugin_type {
                PluginType::Auth => "auth",
                PluginType::Guard => "guard",
                PluginType::Transform => "transform",
            }
            .to_owned(),
        ),
        "tenant_id" => Some(plugin.tenant_id.to_string()),
        _ => None,
    }
}

fn ancestor_configs(
    snapshots: &[AncestorSnapshot],
) -> Vec<crate::domain::merge::AncestorConfig<'_>> {
    snapshots
        .iter()
        .map(AncestorSnapshot::as_ancestor_config)
        .collect()
}

/// Builds the ordered plugin chain of a resolved target.
///
/// Guard and transform bindings come from the merged plugin chain; the single
/// auth binding comes from the merged auth configuration.
#[must_use]
fn build_plugin_chain(plugins: &PluginsConfig, auth: Option<&AuthConfig>) -> PluginChain {
    let mut chain = PluginChain::default();
    for binding in &plugins.items {
        let invocation = PluginInvocation {
            plugin_ref: binding.plugin_ref().to_owned(),
            config: binding.config(),
            from_upstream: true,
        };
        match plugin_kind(binding.plugin_ref()) {
            PluginType::Auth => {
                if chain.auth.is_none() {
                    chain.auth = Some(invocation);
                }
            }
            PluginType::Guard => chain.guards.push(invocation),
            PluginType::Transform => chain.transforms.push(invocation),
        }
    }
    if let Some(auth) = auth
        && let Some(plugin_ref) = auth.plugin_ref()
    {
        chain.auth = Some(PluginInvocation {
            plugin_ref: plugin_ref.to_owned(),
            config: auth
                .config
                .clone()
                .unwrap_or_else(serde_json::Value::default),
            from_upstream: true,
        });
    }
    chain
}

/// Classifies a plugin reference by its GTS base type.
#[must_use]
pub fn plugin_kind(plugin_ref: &str) -> PluginType {
    if plugin_ref.contains("auth_plugin") {
        PluginType::Auth
    } else if plugin_ref.contains("guard_plugin") {
        PluginType::Guard
    } else {
        PluginType::Transform
    }
}

/// Validates every plugin binding of a plugin chain at create/replace time.
///
/// # Errors
///
/// 400 [`DomainError::Validation`] naming the first offending reference.
fn validate_plugin_bindings(plugins: Option<&PluginsConfig>) -> Result<(), DomainError> {
    let Some(plugins) = plugins else {
        return Ok(());
    };
    for binding in &plugins.items {
        validate_plugin_ref(binding.plugin_ref())?;
    }
    Ok(())
}

/// Validates a plugin reference at create time (`DESIGN.md` §3.2).
///
/// * a named reference must carry a plugin type part — `auth_plugin`,
///   `guard_plugin` or `transform_plugin` — and that part must match the slot
///   the reference occupies;
/// * catalog-only identifiers (`cf.core.oagw.basic.v1`, `cf.core.oagw.timeout.v1`,
///   …) are reserved in the types-registry with no backing implementation and
///   **cannot** be bound through `plugins.items[].plugin_ref` or `auth.type`;
/// * a bare UUID names a tenant-defined (Starlark) plugin, whose type is only
///   known to the plugin registry at proxy time.
///
/// Rejecting a bad reference at create time turns a per-request 503
/// (`plugin.not_found`) into a 400 configuration error.
///
/// # Errors
///
/// 400 [`DomainError::Validation`] when the reference is unresolvable, sits in
/// the wrong slot or names a catalog-only plugin.
fn validate_plugin_ref(reference: &str) -> Result<(), DomainError> {
    if reference.is_empty() {
        return Err(DomainError::Validation(
            "plugin_ref must not be empty".to_owned(),
        ));
    }
    // A bare instance id (a tenant-defined plugin UUID) is resolved by the
    // plugin registry, not by the built-in catalog.
    if gts::split_instance_id(reference).is_none() && uuid::Uuid::parse_str(reference).is_ok() {
        return Ok(());
    }
    let kind = plugin_kind(reference);
    let type_part = gts::split_instance_id(reference).map_or(reference, |(type_part, _)| type_part);
    // `base_type_id()` is the `gts.<type>~` form and `type_part` the bare
    // `<type>` form, so both are trimmed before the comparison.
    let expected = kind
        .base_type_id()
        .trim_start_matches("gts.")
        .trim_end_matches('~');
    if !type_part.eq_ignore_ascii_case(expected) {
        let slot = match kind {
            PluginType::Auth => "auth_plugin",
            PluginType::Guard => "guard_plugin",
            PluginType::Transform => "transform_plugin",
        };
        return Err(DomainError::Validation(format!(
            "plugin_ref '{reference}' is not a {slot} reference"
        )));
    }
    let catalog_only: &[&str] = match kind {
        PluginType::Auth => &gts::CATALOG_ONLY_AUTH_PLUGINS,
        PluginType::Guard => &gts::CATALOG_ONLY_GUARD_PLUGINS,
        PluginType::Transform => &gts::CATALOG_ONLY_TRANSFORM_PLUGINS,
    };
    if catalog_only.contains(&reference) {
        return Err(DomainError::Validation(format!(
            "plugin_ref '{reference}' is a catalog-only identifier and cannot be bound"
        )));
    }
    Ok(())
}

/// Validates the `auth.type` slot: it must name an `auth_plugin`.
///
/// The plugin chain (`plugins.items[].plugin_ref`) carries the plugin type in
/// its GTS type part, but `auth.type` is a single slot, so a `guard_plugin` or
/// `transform_plugin` reference there is a configuration error. A bare UUID
/// names a tenant-defined plugin whose type is only known to the plugin
/// registry and is accepted.
///
/// # Errors
///
/// 400 [`DomainError::Validation`] when the reference names a non-auth plugin.
fn validate_auth_slot(reference: &str) -> Result<(), DomainError> {
    validate_plugin_ref(reference)?;
    let tenant_defined =
        gts::split_instance_id(reference).is_none() && uuid::Uuid::parse_str(reference).is_ok();
    if !tenant_defined && plugin_kind(reference) != PluginType::Auth {
        return Err(DomainError::Validation(format!(
            "auth.type '{reference}' must be an auth_plugin reference"
        )));
    }
    Ok(())
}

#[async_trait::async_trait]
impl ControlPlaneService for ControlPlaneServiceImpl {
    async fn create_upstream(
        &self,
        ctx: &SecurityContext,
        mut upstream: Upstream,
    ) -> Result<Upstream, DomainError> {
        self.ensure_permission(ctx, gts::UPSTREAM_TYPE_ID, Permission::Create)
            .await?;
        let tenant_id = Self::caller_tenant(ctx)?;
        upstream.tenant_id = tenant_id;
        upstream.id = uuid::Uuid::new_v4();
        upstream.created_at = crate::domain::model::now_millis();
        Self::validate_upstream(&upstream, self.allow_http_upstream)?;
        let provided = (!upstream.alias.is_empty()).then_some(upstream.alias.as_str());
        upstream.alias = alias::enforce_alias_on_create(&upstream.server.endpoints, provided)?;
        self.ensure_unique_alias(tenant_id, &upstream, None)?;
        let chain = self.tenant_chain(ctx).await?;
        self.ensure_ancestor_bind_allowed(ctx, &chain, &upstream)
            .await?;
        self.store.put_upstream(upstream.clone())?;
        Ok(upstream)
    }

    async fn list_upstreams(
        &self,
        ctx: &SecurityContext,
        query: &ListQuery,
    ) -> Result<Vec<Upstream>, DomainError> {
        self.ensure_permission(ctx, gts::UPSTREAM_TYPE_ID, Permission::Read)
            .await?;
        let tenant_id = Self::caller_tenant(ctx)?;
        let mut items = self.store.upstreams(tenant_id);
        items.sort_by_key(|upstream| upstream.created_at);
        self.list_filtered(items, query, upstream_field)
    }

    async fn get_upstream(&self, ctx: &SecurityContext, id: &str) -> Result<Upstream, DomainError> {
        self.ensure_permission(ctx, gts::UPSTREAM_TYPE_ID, Permission::Read)
            .await?;
        let tenant_id = Self::caller_tenant(ctx)?;
        let parsed = Self::parse_id(id)
            .ok_or_else(|| DomainError::Validation(format!("'{id}' is not an upstream id")))?;
        self.store
            .upstream(tenant_id, parsed)
            .ok_or_else(|| DomainError::RouteNotFound(format!("no upstream '{id}' in this tenant")))
    }

    async fn replace_upstream(
        &self,
        ctx: &SecurityContext,
        id: &str,
        mut upstream: Upstream,
    ) -> Result<Upstream, DomainError> {
        self.ensure_permission(ctx, gts::UPSTREAM_TYPE_ID, Permission::Override)
            .await?;
        let tenant_id = Self::caller_tenant(ctx)?;
        let parsed = Self::parse_id(id)
            .ok_or_else(|| DomainError::Validation(format!("'{id}' is not an upstream id")))?;
        let existing = self.store.upstream(tenant_id, parsed).ok_or_else(|| {
            DomainError::RouteNotFound(format!("no upstream '{id}' in this tenant"))
        })?;
        Self::validate_upstream(&upstream, self.allow_http_upstream)?;
        upstream.id = existing.id;
        upstream.tenant_id = existing.tenant_id;
        upstream.created_at = existing.created_at;
        let provided = (!upstream.alias.is_empty()).then_some(upstream.alias.as_str());
        upstream.alias = alias::enforce_alias_update_with(
            &existing.alias,
            &upstream.server.endpoints,
            provided,
        )?;
        self.ensure_unique_alias(tenant_id, &upstream, Some(existing.id))?;
        let chain = self.tenant_chain(ctx).await?;
        self.ensure_ancestor_bind_allowed(ctx, &chain, &upstream)
            .await?;
        self.store.save_upstream(upstream.clone())?;
        Ok(upstream)
    }

    async fn delete_upstream(&self, ctx: &SecurityContext, id: &str) -> Result<(), DomainError> {
        self.ensure_permission(ctx, gts::UPSTREAM_TYPE_ID, Permission::Delete)
            .await?;
        let tenant_id = Self::caller_tenant(ctx)?;
        let parsed = Self::parse_id(id)
            .ok_or_else(|| DomainError::Validation(format!("'{id}' is not an upstream id")))?;
        if self.store.upstream(tenant_id, parsed).is_none() {
            return Err(DomainError::RouteNotFound(format!(
                "no upstream '{id}' in this tenant"
            )));
        }
        if !self.store.routes_of_upstream(parsed).is_empty() {
            return Err(DomainError::Conflict(format!(
                "upstream '{id}' still has routes; delete them first"
            )));
        }
        if !self.store.remove_upstream(tenant_id, parsed) {
            return Err(DomainError::RouteNotFound(format!(
                "no upstream '{id}' in this tenant"
            )));
        }
        Ok(())
    }

    async fn create_route(
        &self,
        ctx: &SecurityContext,
        mut route: Route,
    ) -> Result<Route, DomainError> {
        self.ensure_permission(ctx, gts::ROUTE_TYPE_ID, Permission::Create)
            .await?;
        let tenant_id = Self::caller_tenant(ctx)?;
        route.tenant_id = tenant_id;
        route.id = uuid::Uuid::new_v4();
        route.created_at = crate::domain::model::now_millis();
        Self::validate_route(&route)?;
        if self.store.upstream(tenant_id, route.upstream_id).is_none() {
            return Err(DomainError::Validation(format!(
                "upstream '{}' is unknown in this tenant",
                route.upstream_id
            )));
        }
        self.ensure_unique_match(route.upstream_id, &route, None)?;
        self.store.put_route(route.clone())?;
        Ok(route)
    }

    async fn list_routes(
        &self,
        ctx: &SecurityContext,
        query: &ListQuery,
    ) -> Result<Vec<Route>, DomainError> {
        self.ensure_permission(ctx, gts::ROUTE_TYPE_ID, Permission::Read)
            .await?;
        let tenant_id = Self::caller_tenant(ctx)?;
        let mut items = self.store.routes(tenant_id);
        items.sort_by_key(|route| route.created_at);
        self.list_filtered(items, query, route_field)
    }

    async fn get_route(&self, ctx: &SecurityContext, id: &str) -> Result<Route, DomainError> {
        self.ensure_permission(ctx, gts::ROUTE_TYPE_ID, Permission::Read)
            .await?;
        let tenant_id = Self::caller_tenant(ctx)?;
        let parsed = Self::parse_id(id)
            .ok_or_else(|| DomainError::Validation(format!("'{id}' is not a route id")))?;
        self.store
            .route(tenant_id, parsed)
            .ok_or_else(|| DomainError::RouteNotFound(format!("no route '{id}' in this tenant")))
    }

    async fn replace_route(
        &self,
        ctx: &SecurityContext,
        id: &str,
        mut route: Route,
    ) -> Result<Route, DomainError> {
        self.ensure_permission(ctx, gts::ROUTE_TYPE_ID, Permission::Override)
            .await?;
        let tenant_id = Self::caller_tenant(ctx)?;
        let parsed = Self::parse_id(id)
            .ok_or_else(|| DomainError::Validation(format!("'{id}' is not a route id")))?;
        let existing = self
            .store
            .route(tenant_id, parsed)
            .ok_or_else(|| DomainError::RouteNotFound(format!("no route '{id}' in this tenant")))?;
        Self::validate_route(&route)?;
        route.id = existing.id;
        route.tenant_id = existing.tenant_id;
        route.upstream_id = existing.upstream_id;
        route.created_at = existing.created_at;
        self.ensure_unique_match(route.upstream_id, &route, Some(existing.id))?;
        self.store.save_route(route.clone())?;
        Ok(route)
    }

    async fn delete_route(&self, ctx: &SecurityContext, id: &str) -> Result<(), DomainError> {
        self.ensure_permission(ctx, gts::ROUTE_TYPE_ID, Permission::Delete)
            .await?;
        let tenant_id = Self::caller_tenant(ctx)?;
        let parsed = Self::parse_id(id)
            .ok_or_else(|| DomainError::Validation(format!("'{id}' is not a route id")))?;
        if self.store.route(tenant_id, parsed).is_none() {
            return Err(DomainError::RouteNotFound(format!(
                "no route '{id}' in this tenant"
            )));
        }
        if !self.store.remove_route(tenant_id, parsed) {
            return Err(DomainError::RouteNotFound(format!(
                "no route '{id}' in this tenant"
            )));
        }
        Ok(())
    }

    async fn create_plugin(
        &self,
        ctx: &SecurityContext,
        mut plugin: Plugin,
    ) -> Result<Plugin, DomainError> {
        let tenant_id = Self::caller_tenant(ctx)?;
        self.ensure_permission(ctx, plugin.plugin_type.base_type_id(), Permission::Create)
            .await?;
        plugin.tenant_id = tenant_id;
        plugin.id = uuid::Uuid::new_v4();
        plugin.created_at = crate::domain::model::now_millis();
        if plugin.name.trim().is_empty() {
            return Err(DomainError::Validation(
                "plugin name is required".to_owned(),
            ));
        }
        if plugin.source_code.trim().is_empty() {
            return Err(DomainError::Validation(
                "plugin source_code is required".to_owned(),
            ));
        }
        self.ensure_unique_plugin_name(tenant_id, &plugin, None)?;
        self.store.put_plugin(plugin.clone())?;
        Ok(plugin)
    }

    async fn list_plugins(
        &self,
        ctx: &SecurityContext,
        query: &ListQuery,
        plugin_type: Option<PluginType>,
    ) -> Result<Vec<Plugin>, DomainError> {
        self.ensure_permission(
            ctx,
            plugin_type.map_or(gts::TRANSFORM_PLUGIN_TYPE_ID, PluginType::base_type_id),
            Permission::Read,
        )
        .await?;
        let tenant_id = Self::caller_tenant(ctx)?;
        let mut items = self.store.plugins(tenant_id, plugin_type);
        items.sort_by_key(|plugin| plugin.created_at);
        self.list_filtered(items, query, plugin_field)
    }

    async fn get_plugin(&self, ctx: &SecurityContext, id: &str) -> Result<Plugin, DomainError> {
        let tenant_id = Self::caller_tenant(ctx)?;
        let parsed = Self::parse_id(id)
            .ok_or_else(|| DomainError::Validation(format!("'{id}' is not a plugin id")))?;
        let plugin = self
            .store
            .plugin(parsed)
            .filter(|plugin| plugin.tenant_id == tenant_id)
            .ok_or_else(|| {
                DomainError::PluginNotFound(format!("no plugin '{id}' in this tenant"))
            })?;
        self.ensure_permission(ctx, plugin.plugin_type.base_type_id(), Permission::Read)
            .await?;
        Ok(plugin)
    }

    async fn delete_plugin(&self, ctx: &SecurityContext, id: &str) -> Result<(), DomainError> {
        let tenant_id = Self::caller_tenant(ctx)?;
        let parsed = Self::parse_id(id)
            .ok_or_else(|| DomainError::Validation(format!("'{id}' is not a plugin id")))?;
        let Some(plugin) = self.store.plugin(parsed) else {
            return Err(DomainError::PluginNotFound(format!(
                "no plugin '{id}' in this tenant"
            )));
        };
        if plugin.tenant_id != tenant_id {
            return Err(DomainError::PluginNotFound(format!(
                "no plugin '{id}' in this tenant"
            )));
        }
        self.ensure_permission(ctx, plugin.plugin_type.base_type_id(), Permission::Delete)
            .await?;
        let parsed = Self::parse_id(id)
            .ok_or_else(|| DomainError::Validation(format!("'{id}' is not a plugin id")))?;
        let Some(plugin) = self.store.plugin(parsed) else {
            return Err(DomainError::PluginNotFound(format!(
                "no plugin '{id}' in this tenant"
            )));
        };
        if plugin.tenant_id != tenant_id {
            return Err(DomainError::PluginNotFound(format!(
                "no plugin '{id}' in this tenant"
            )));
        }
        let (upstreams, routes) = self.plugin_references(parsed);
        if !upstreams.is_empty() || !routes.is_empty() {
            return Err(DomainError::PluginInUse {
                detail: format!(
                    "plugin '{}' is still referenced by {} upstream(s) and {} route(s)",
                    plugin.name,
                    upstreams.len(),
                    routes.len()
                ),
                plugin_id: crate::domain::gts::plugin_instance_id(plugin.plugin_type, plugin.id),
                referenced_by: crate::domain::error::ReferencedBy { upstreams, routes },
            });
        }
        self.store.remove_plugin(parsed);
        Ok(())
    }

    async fn get_plugin_source(
        &self,
        ctx: &SecurityContext,
        id: &str,
    ) -> Result<PluginSource, DomainError> {
        let plugin = self.get_plugin(ctx, id).await?;
        Ok(PluginSource {
            plugin_id: plugin.id.to_string(),
            plugin_type: plugin.plugin_type,
            source_code: plugin.source_code,
        })
    }

    async fn resolve_proxy_target(
        &self,
        ctx: &SecurityContext,
        alias_value: &str,
        method: &str,
        request_path: &str,
        _path_suffix: &str,
    ) -> Result<ResolvedTarget, DomainError> {
        self.ensure_permission(ctx, PROXY_TYPE_ID, Permission::Invoke)
            .await?;
        let caller = Self::caller_tenant(ctx)?;
        let chain = self.tenant_chain(ctx).await?;
        let normalized = alias::normalize(alias_value);
        let selected = match self.resolve_alias(&chain, &normalized) {
            AliasSelection::Resolved(upstream) => *upstream,
            AliasSelection::Disabled => {
                return Err(DomainError::UpstreamDisabled {
                    alias: normalized,
                    retry_after_seconds: UPSTREAM_DISABLED_RETRY_AFTER_SECS,
                });
            }
            AliasSelection::Absent => {
                return Err(DomainError::RouteNotFound(format!(
                    "no enabled upstream with alias '{alias_value}'"
                )));
            }
        };
        let snapshots = self.ancestor_snapshots(&chain, &normalized, selected.tenant_id);

        // Route matching walks the alias candidates from the closest tenant to
        // the root, so a descendant route wins over an equally good ancestor one.
        let mut winner: Option<Route> = None;
        for upstream in std::iter::once(&selected)
            .chain(snapshots.iter().rev().map(|snapshot| &snapshot.upstream))
        {
            let routes = self.store.routes_of_upstream(upstream.id);
            let candidates_routes: Vec<RouteCandidate<'_>> = routes
                .iter()
                .map(|route| RouteCandidate {
                    tenant_id: route.tenant_id,
                    upstream_id: route.upstream_id,
                    route,
                })
                .collect();
            if let Ok(route) =
                crate::domain::routing::match_http_route(&candidates_routes, method, request_path)
            {
                winner = Some(route.clone());
                break;
            }
        }
        let Some(route) = winner else {
            return Err(DomainError::RouteNotFound(format!(
                "no route matches {method} {request_path} for alias '{alias_value}'"
            )));
        };

        let ancestors = ancestor_configs(&snapshots);
        let headers: Option<HeadersConfig> =
            crate::domain::merge::effective_headers(&ancestors, selected.headers.as_ref());
        let plugins: PluginsConfig = crate::domain::merge::effective_plugins(
            &ancestors,
            selected.plugins.as_ref(),
            route.plugins.as_ref(),
        );
        let cors: Option<CorsConfig> = crate::domain::merge::effective_cors(
            &ancestors,
            selected.cors.as_ref(),
            route.cors.as_ref(),
        );
        let rate_limit: Option<RateLimitConfig> = crate::domain::merge::effective_rate_limit(
            &ancestors,
            selected.rate_limit.as_ref(),
            route.rate_limit.as_ref(),
        );
        let auth: Option<AuthConfig> =
            crate::domain::merge::effective_auth(&ancestors, selected.auth.as_ref()).cloned();
        let alias_is_common_suffix = Self::alias_is_common_suffix(&selected);

        Ok(ResolvedTarget {
            inherited: selected.tenant_id != caller,
            upstream: selected,
            chain: snapshots,
            route_inherited: route.tenant_id != caller,
            route,
            plugin_chain: build_plugin_chain(&plugins, auth.as_ref()),
            rate_limit,
            cors,
            headers,
            alias_is_common_suffix,
        })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    /// An upstream shell with `alias`, no configuration and one endpoint.
    fn upstream(tenant_id: uuid::Uuid, alias: &str) -> Upstream {
        Upstream {
            id: uuid::Uuid::new_v4(),
            tenant_id,
            alias: alias.to_owned(),
            protocol: gts::PROTOCOL_HTTP.to_owned(),
            enabled: true,
            server: crate::domain::model::ServerConfig {
                endpoints: vec![crate::domain::model::Endpoint {
                    scheme: crate::domain::model::EndpointScheme::Https,
                    host: format!("{alias}.example.com"),
                    port: 443,
                }],
            },
            auth: None,
            headers: None,
            rate_limit: None,
            cors: None,
            plugins: None,
            tags: Vec::new(),
            created_at: 0,
        }
    }

    fn root_child() -> (uuid::Uuid, uuid::Uuid) {
        (uuid::Uuid::new_v4(), uuid::Uuid::new_v4())
    }

    #[test]
    fn an_alias_held_by_a_disabled_ancestor_is_disabled() {
        let (root, child) = root_child();
        let service = ControlPlaneServiceImpl::new(InMemoryStore::new(), None);
        let mut ancestor = upstream(root, "vendor.com");
        ancestor.enabled = false;
        service.store.put_upstream(ancestor).unwrap();
        service
            .store
            .put_upstream(upstream(child, "vendor.com"))
            .unwrap();

        assert!(matches!(
            service.resolve_alias(&[root, child], "vendor.com"),
            AliasSelection::Disabled
        ));
    }

    #[test]
    fn the_closest_tenant_wins_the_alias_lookup() {
        let (root, child) = root_child();
        let service = ControlPlaneServiceImpl::new(InMemoryStore::new(), None);
        let ancestor = upstream(root, "vendor.com");
        let descendant = upstream(child, "vendor.com");
        service.store.put_upstream(ancestor).unwrap();
        service.store.put_upstream(descendant.clone()).unwrap();

        match service.resolve_alias(&[root, child], "vendor.com") {
            AliasSelection::Resolved(selected) => assert_eq!(selected.id, descendant.id),
            other => panic!("expected a resolved alias, got {other:?}"),
        }
        assert!(matches!(
            service.resolve_alias(&[root, child], "absent.example.com"),
            AliasSelection::Absent
        ));
    }

    #[test]
    fn a_disabled_ancestor_is_dropped_from_the_snapshots() {
        let (root, child, grandchild) = (
            uuid::Uuid::new_v4(),
            uuid::Uuid::new_v4(),
            uuid::Uuid::new_v4(),
        );
        let service = ControlPlaneServiceImpl::new(InMemoryStore::new(), None);
        let mut disabled = upstream(root, "vendor.com");
        disabled.enabled = false;
        let middle = upstream(child, "vendor.com");
        service.store.put_upstream(disabled).unwrap();
        service.store.put_upstream(middle.clone()).unwrap();
        service
            .store
            .put_upstream(upstream(grandchild, "vendor.com"))
            .unwrap();

        let snapshots =
            service.ancestor_snapshots(&[root, child, grandchild], "vendor.com", grandchild);
        assert_eq!(
            snapshots
                .iter()
                .map(|snapshot| snapshot.tenant_id)
                .collect::<Vec<_>>(),
            vec![child],
            "the disabled ancestor contributes no constraints"
        );
        assert_eq!(snapshots[0].upstream.id, middle.id);
    }

    #[test]
    fn snapshots_are_empty_for_an_unknown_selected_tenant() {
        let service = ControlPlaneServiceImpl::new(InMemoryStore::new(), None);
        assert!(
            service
                .ancestor_snapshots(&[uuid::Uuid::new_v4()], "vendor.com", uuid::Uuid::new_v4())
                .is_empty()
        );
    }

    #[test]
    fn catalog_only_plugin_references_are_rejected() {
        for reference in [
            gts::AUTH_PLUGIN_BASIC,
            gts::AUTH_PLUGIN_BEARER,
            gts::GUARD_PLUGIN_TIMEOUT,
            gts::GUARD_PLUGIN_CORS,
            gts::TRANSFORM_PLUGIN_LOGGING,
        ] {
            let error = validate_plugin_ref(reference).expect_err(reference);
            assert_eq!(error.status(), 400, "{reference}");
            assert!(
                error.detail().contains("catalog-only"),
                "{}: {}",
                reference,
                error.detail()
            );
        }
        for reference in [
            gts::AUTH_PLUGIN_NOOP,
            gts::AUTH_PLUGIN_APIKEY,
            gts::GUARD_PLUGIN_REQUIRED_HEADERS,
            gts::TRANSFORM_PLUGIN_REQUEST_ID,
        ] {
            assert!(validate_plugin_ref(reference).is_ok(), "{reference}");
        }
    }

    #[test]
    fn a_plugin_reference_in_the_wrong_slot_is_rejected() {
        // A guard reference is legal in the plugin chain...
        assert!(validate_plugin_ref(gts::GUARD_PLUGIN_REQUIRED_HEADERS).is_ok());
        // ...but not in the `auth.type` slot.
        let error = validate_auth_slot(gts::GUARD_PLUGIN_REQUIRED_HEADERS).expect_err("guard ref");
        assert_eq!(error.status(), 400);
        assert!(error.detail().contains("auth_plugin"), "{}", error.detail());
        // A bare built-in instance id is equally unusable there.
        let error = validate_auth_slot(gts::TRANSFORM_PLUGIN_REQUEST_ID_INSTANCE)
            .expect_err("bare transform instance");
        assert_eq!(error.status(), 400);
        assert!(validate_auth_slot(gts::AUTH_PLUGIN_NOOP).is_ok());
        let tenant_defined = uuid::Uuid::new_v4().to_string();
        assert!(validate_auth_slot(&tenant_defined).is_ok());
    }

    #[test]
    fn a_tenant_defined_plugin_reference_is_accepted_without_a_catalog() {
        let reference = uuid::Uuid::new_v4().to_string();
        assert!(validate_plugin_ref(&reference).is_ok());
        // A named reference that is neither a GTS identifier nor a UUID is not
        // resolvable by the plugin registry either.
        let error = validate_plugin_ref("my-plugin").expect_err("named reference");
        assert_eq!(error.status(), 400);
    }

    #[test]
    fn permission_actions_follow_the_design_table() {
        assert_eq!(Permission::Create.action(), "create");
        assert_eq!(Permission::Override.action(), "override");
        assert_eq!(Permission::Read.action(), "read");
        assert_eq!(Permission::Delete.action(), "delete");
        assert_eq!(Permission::Bind.action(), "bind");
        assert_eq!(Permission::Invoke.action(), "invoke");
    }

    #[test]
    fn list_filtered_applies_orderby_skip_and_top() {
        let service = ControlPlaneServiceImpl::new(InMemoryStore::new(), None);
        let items = vec!["bravo", "alpha", "charlie"];
        let query = ListQuery {
            filter: None,
            orderby: Some("value".to_owned()),
            skip: Some(1),
            top: Some(1),
            select: None,
        };
        let page = service
            .list_filtered(items, &query, |item: &&str, name| {
                (name == "value").then(|| (*item).to_owned())
            })
            .unwrap();
        assert_eq!(page, vec!["bravo"]);

        let query = ListQuery {
            skip: None,
            top: Some(1),
            orderby: Some("value desc".to_owned()),
            ..query
        };
        let page = service
            .list_filtered(
                vec!["bravo", "alpha", "charlie"],
                &query,
                |item: &&str, name| (name == "value").then(|| (*item).to_owned()),
            )
            .unwrap();
        assert_eq!(page, vec!["charlie"]);
    }
}
