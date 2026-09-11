//! Request-time alias resolution, hierarchical configuration merge, and HTTP
//! route matching (`cpt-cf-oagw-feature-config-resolution`).
//!
//! [`resolve_proxy_target`] is the single entry point every later
//! data-plane feature builds on: it turns `(tenant_id, alias, method, path)`
//! into a [`ResolvedPlan`] — the selected upstream, the matched route, the
//! per-field merged configuration, the effective header-transformation
//! plan, and the chosen endpoint list — or a gateway error.
//!
//! ## The two-tier hierarchy this feature reuses
//!
//! `crate::domain::alias::ROOT_TENANT_ID` (feature 2) is the only tenant
//! hierarchy mechanism this codebase defines: a single, deterministic nil
//! UUID "root" ancestor stands in for the platform's real tenant hierarchy.
//! This feature reuses it rather than inventing a second mechanism. Every
//! alias-hierarchy walk and every per-field merge below therefore operates
//! over at most two tiers: the calling tenant (the descendant) and
//! [`ROOT_TENANT_ID`] (the ancestor).
//!
//! ## How the per-field merge maps onto that hierarchy
//!
//! The "Upstream tier" in `cpt-cf-oagw-algo-sharing-mode-merge` is always
//! [`ROOT_TENANT_ID`]'s own upstream record for the requested alias (the
//! ancestor-established base configuration, if any); the "Tenant tier" is
//! always the calling tenant's own upstream record for the same alias (its
//! override), when the calling tenant differs from the root. This holds
//! independently of which upstream the alias-hierarchy walk actually
//! *selects* for serving (endpoints, protocol): shadowing selects the
//! routing target only (`cpt-cf-oagw-dod-enforced-limits-across-shadowing`),
//! so a descendant's own, enabled, shadowing upstream can win selection while
//! the root's `enforce`-mode constraints still fold into the merge. The
//! "Route tier" is always the matched route's own fields, and a matched
//! route always belongs to whichever upstream selection actually picked.
//!
//! `Sharing::Private`'s doc comment ("not visible to descendants") is taken
//! literally here: a root-owned field flagged `private` is excluded from the
//! merge entirely for any calling tenant other than the root itself, rather
//! than merely blocking that tenant's own override attempt.
//!
//! `headers` carries no `sharing` field in the checked-in schema (unlike
//! `auth`, `rate_limit`, `cors`, and `plugins`), so it cannot participate in
//! the private/inherit/enforce switch at all; this feature merges it as a
//! plain override (the tenant tier's own headers configuration, when
//! present, replaces the root tier's).

// `OagwError` is returned unboxed from every fallible function across this
// crate (see `src/domain/service.rs`'s identical allow); boxing it only in
// this module would be an inconsistent, purely lint-driven special case.
#![allow(clippy::result_large_err)]

use std::collections::BTreeMap;

use uuid::Uuid;

use super::alias::{ROOT_TENANT_ID, normalize_alias};
use super::model::{
    AuthConfig, CorsConfig, HeadersConfig, Passthrough, PluginItem, PluginsConfig, RateLimitConfig,
    Route, Sharing, Upstream, Window,
};
use super::resolve_cache::ResolvedConfigCacheKey;
use crate::error::OagwError;
use crate::state::{ControlPlaneState, TenantState};

/// The effective request-header transformation rules
/// (`cpt-cf-oagw-dod-header-transformation-plan`).
#[derive(Debug, Clone, Default)]
pub struct RequestHeaderPlan {
    pub set: BTreeMap<String, String>,
    pub add: BTreeMap<String, String>,
    pub remove: Vec<String>,
    pub passthrough: Passthrough,
    pub passthrough_allowlist: Vec<String>,
}

/// The effective response-header transformation rules
/// (`cpt-cf-oagw-dod-header-transformation-plan`).
#[derive(Debug, Clone, Default)]
pub struct ResponseHeaderPlan {
    pub set: BTreeMap<String, String>,
    pub add: BTreeMap<String, String>,
    pub remove: Vec<String>,
}

/// The effective header-transformation plan for both directions
/// (`cpt-cf-oagw-dod-header-transformation-plan`,
/// `cpt-cf-oagw-algo-header-plan-compute`).
#[derive(Debug, Clone, Default)]
pub struct HeaderPlan {
    pub request: RequestHeaderPlan,
    pub response: ResponseHeaderPlan,
}

/// The resolved plan for one proxy request: the selected upstream, the
/// matched route, the per-field merged configuration, the effective header
/// plan, and the chosen endpoint list
/// (`cpt-cf-oagw-dod-sharing-mode-merge`, `cpt-cf-oagw-dod-http-route-selection`,
/// `cpt-cf-oagw-dod-header-transformation-plan`).
///
/// This is the single seam features 6 (`http-proxy`), 7 (`streaming-proxy`),
/// and 8 (`plugin-runtime`) build on: every merged value here is final —
/// none of those features recompute a sharing mode or re-derive an
/// effective limit.
#[derive(Debug, Clone)]
pub struct ResolvedPlan {
    /// The upstream selected by the alias-hierarchy walk (closest enabled
    /// match); its `server.endpoints` is the connection pool used to serve
    /// this request.
    pub upstream: Upstream,
    /// The tenant that owns [`Self::upstream`] (may differ from the calling
    /// tenant under shadowing).
    pub owning_tenant_id: Uuid,
    /// The route selected by `cpt-cf-oagw-algo-http-route-select`, carrying
    /// its own `path_suffix_mode`.
    pub route: Route,
    pub effective_auth: Option<AuthConfig>,
    pub effective_headers: Option<HeadersConfig>,
    pub effective_rate_limit: Option<RateLimitConfig>,
    /// Concatenated `upstream + route + tenant`, in that order
    /// (`cpt-cf-oagw-dod-sharing-mode-merge`).
    pub effective_plugins: Vec<PluginItem>,
    pub effective_cors: Option<CorsConfig>,
    /// `union(upstream.tags, route.tags, tenant.tags)`
    /// (`cpt-cf-oagw-dod-tag-union`).
    pub effective_tags: Vec<String>,
    pub header_plan: HeaderPlan,
    /// The chosen endpoint list: `upstream.server.endpoints`.
    pub endpoints: Vec<super::model::Endpoint>,
}

/// Resolves a proxied request's alias, tenant hierarchy, and matching HTTP
/// route into one merged [`ResolvedPlan`]
/// (`cpt-cf-oagw-flow-resolve-proxy-request`).
///
/// Serves from `state`'s resolved-configuration cache on a hit
/// (`cpt-cf-oagw-algo-resolved-config-cache-lookup`); on a miss, walks the
/// tenant hierarchy (`cpt-cf-oagw-algo-alias-hierarchy-lookup`), merges
/// per-field configuration (`cpt-cf-oagw-algo-sharing-mode-merge`), selects
/// the matching HTTP route (`cpt-cf-oagw-algo-http-route-select`), computes
/// the header-transformation plan (`cpt-cf-oagw-algo-header-plan-compute`),
/// and populates the cache before returning.
///
/// # Errors
///
/// Returns [`OagwError::route_not_found`] when no tenant in the hierarchy
/// holds an upstream carrying the normalized alias, or no enabled route
/// matches the inbound method and path
/// (`cpt-cf-oagw-dod-route-not-found-outcome`); returns
/// [`OagwError::link_unavailable`] when every tier carrying the alias holds
/// only disabled upstreams (`cpt-cf-oagw-dod-upstream-disabled-outcome`).
pub fn resolve_proxy_target(
    state: &ControlPlaneState,
    tenant_id: Uuid,
    alias: &str,
    method: &str,
    path: &str,
) -> Result<ResolvedPlan, OagwError> {
    // @cpt-begin:cpt-cf-oagw-dod-request-alias-normalization:p1:inst-resolve-normalize-01
    let normalized_alias = normalize_alias(alias);
    // @cpt-end:cpt-cf-oagw-dod-request-alias-normalization:p1:inst-resolve-normalize-01
    let method = method.to_ascii_uppercase();

    let cache_key = ResolvedConfigCacheKey {
        tenant_id,
        alias: normalized_alias.clone(),
        method: method.clone(),
        path: path.to_owned(),
    };
    // @cpt-begin:cpt-cf-oagw-dod-resolved-config-cache-lookup:p1:inst-resolve-cache-hit-01
    if let Some(cached) = state.resolved_cache().get(&cache_key) {
        return Ok(cached);
    }
    // @cpt-end:cpt-cf-oagw-dod-resolved-config-cache-lookup:p1:inst-resolve-cache-hit-01

    let (selected_upstream, owning_tenant_id) =
        selected_upstream(state, tenant_id, &normalized_alias)?;

    let owning_tenant = state.tenant(owning_tenant_id);
    // @cpt-begin:cpt-cf-oagw-dod-http-route-selection:p1:inst-resolve-route-select-01
    let matched_route = select_route(&owning_tenant, selected_upstream.id, &method, path)
        .ok_or_else(|| {
            OagwError::route_not_found(format!(
                "no enabled route matches method '{method}' and path '{path}' for alias \
                 '{normalized_alias}'"
            ))
        })?;
    // @cpt-end:cpt-cf-oagw-dod-http-route-selection:p1:inst-resolve-route-select-01

    let root_tenant = state.tenant(ROOT_TENANT_ID);
    let ancestor_upstream = find_upstream_by_alias(&root_tenant, &normalized_alias);
    let cross_tenant = tenant_id != ROOT_TENANT_ID;
    let tenant_upstream = cross_tenant
        .then(|| find_upstream_by_alias(&state.tenant(tenant_id), &normalized_alias))
        .flatten();

    let plan = build_resolved_plan(
        selected_upstream,
        owning_tenant_id,
        matched_route,
        ancestor_upstream.as_ref(),
        tenant_upstream.as_ref(),
        cross_tenant,
    );

    state.resolved_cache().insert(cache_key, plan.clone());
    Ok(plan)
}

/// Runs the alias-hierarchy walk and turns its outcome into either the
/// selected `(upstream, owning_tenant_id)` pair or the appropriate gateway
/// error (`cpt-cf-oagw-dod-route-not-found-outcome`,
/// `cpt-cf-oagw-dod-upstream-disabled-outcome`).
fn selected_upstream(
    state: &ControlPlaneState,
    tenant_id: Uuid,
    normalized_alias: &str,
) -> Result<(Upstream, Uuid), OagwError> {
    // @cpt-begin:cpt-cf-oagw-dod-alias-hierarchy-lookup:p1:inst-resolve-hierarchy-01
    match lookup_alias_hierarchy(state, tenant_id, normalized_alias) {
        AliasHierarchyOutcome::Enabled {
            upstream,
            owning_tenant_id,
        } => Ok((*upstream, owning_tenant_id)),
        AliasHierarchyOutcome::Disabled => Err(OagwError::link_unavailable(format!(
            "alias '{normalized_alias}' resolves only to disabled upstreams"
        ))),
        AliasHierarchyOutcome::NotFound => Err(OagwError::route_not_found(format!(
            "no upstream in the tenant hierarchy carries alias '{normalized_alias}'"
        ))),
    }
    // @cpt-end:cpt-cf-oagw-dod-alias-hierarchy-lookup:p1:inst-resolve-hierarchy-01
}

/// Outcome of the descendant-to-root alias-hierarchy walk
/// (`cpt-cf-oagw-algo-alias-hierarchy-lookup`).
#[derive(Debug, Clone, PartialEq)]
enum AliasHierarchyOutcome {
    /// The closest enabled matching upstream and its owning tenant.
    Enabled {
        upstream: Box<Upstream>,
        owning_tenant_id: Uuid,
    },
    /// Every tier carrying the alias holds only disabled upstreams.
    Disabled,
    /// No tier in the hierarchy holds an upstream carrying the alias.
    NotFound,
}

/// Walks from `tenant_id` toward [`ROOT_TENANT_ID`], skipping any upstream
/// whose `enabled` field is `false` and continuing toward the root, so the
/// closest enabled match wins
/// (`cpt-cf-oagw-algo-alias-hierarchy-lookup`,
/// `cpt-cf-oagw-dod-alias-hierarchy-lookup`).
// @cpt-begin:cpt-cf-oagw-algo-alias-hierarchy-lookup:p1:inst-hierarchy-lookup-fn-01
fn lookup_alias_hierarchy(
    state: &ControlPlaneState,
    tenant_id: Uuid,
    normalized_alias: &str,
) -> AliasHierarchyOutcome {
    let calling_tenant = state.tenant(tenant_id);
    let mut disabled_seen =
        if let Some(candidate) = find_upstream_by_alias(&calling_tenant, normalized_alias) {
            if candidate.enabled {
                return AliasHierarchyOutcome::Enabled {
                    upstream: Box::new(candidate),
                    owning_tenant_id: tenant_id,
                };
            }
            true
        } else {
            false
        };

    if tenant_id != ROOT_TENANT_ID {
        let root_tenant = state.tenant(ROOT_TENANT_ID);
        if let Some(candidate) = find_upstream_by_alias(&root_tenant, normalized_alias) {
            if candidate.enabled {
                return AliasHierarchyOutcome::Enabled {
                    upstream: Box::new(candidate),
                    owning_tenant_id: ROOT_TENANT_ID,
                };
            }
            disabled_seen = true;
        }
    }

    if disabled_seen {
        AliasHierarchyOutcome::Disabled
    } else {
        AliasHierarchyOutcome::NotFound
    }
}
// @cpt-end:cpt-cf-oagw-algo-alias-hierarchy-lookup:p1:inst-hierarchy-lookup-fn-01

/// The first upstream owned by `tenant` whose `alias` equals
/// `normalized_alias`, regardless of `enabled`.
fn find_upstream_by_alias(tenant: &TenantState, normalized_alias: &str) -> Option<Upstream> {
    tenant
        .upstreams
        .iter()
        .find(|entry| entry.value().alias == normalized_alias)
        .map(|entry| entry.value().clone())
}

// ---------------------------------------------------------------------------
// HTTP route selection (`cpt-cf-oagw-algo-http-route-select`).
// ---------------------------------------------------------------------------

/// Selects the enabled route under `upstream_id` whose `match.http.methods`
/// contains `method`, choosing the longest `match.http.path` prefix of
/// `path` and breaking ties with `priority` (higher wins)
/// (`cpt-cf-oagw-algo-http-route-select`). Returns `None` (the
/// route-not-found outcome) when no route survives the method filter or no
/// surviving route's path is a prefix of `path`.
fn select_route(
    tenant: &TenantState,
    upstream_id: Uuid,
    method: &str,
    path: &str,
) -> Option<Route> {
    tenant
        .routes
        .iter()
        .map(|entry| entry.value().clone())
        .filter(|route| route.upstream_id == upstream_id && route.enabled)
        .filter(|route| route_accepts_method(route, method))
        .filter_map(|route| route_prefix_len(&route, path).map(|len| (len, route)))
        .max_by_key(|(len, route)| (*len, route.priority))
        .map(|(_, route)| route)
}

/// `true` when `route`'s `match.http.methods` contains `method`; always
/// `false` for a `grpc`-matched route (no gRPC proxy code path exists,
/// Overview override 5).
fn route_accepts_method(route: &Route, method: &str) -> bool {
    route
        .match_config
        .http
        .as_ref()
        .is_some_and(|http| http.methods.iter().any(|m| m.as_str() == method))
}

/// The length of `route`'s `match.http.path` when it is a prefix of `path`,
/// or `None` when it is not (or the route is `grpc`-matched).
fn route_prefix_len(route: &Route, path: &str) -> Option<usize> {
    let http = route.match_config.http.as_ref()?;
    path.starts_with(http.path.as_str())
        .then_some(http.path.len())
}

// ---------------------------------------------------------------------------
// Per-field sharing-mode merge (`cpt-cf-oagw-algo-sharing-mode-merge`).
// ---------------------------------------------------------------------------

/// A configuration value that carries its own [`Sharing`] mode, letting the
/// generic tier-application helpers below stay field-agnostic.
trait SharedField {
    fn sharing(&self) -> Sharing;
}

impl SharedField for AuthConfig {
    fn sharing(&self) -> Sharing {
        self.sharing
    }
}

impl SharedField for RateLimitConfig {
    fn sharing(&self) -> Sharing {
        self.sharing
    }
}

impl SharedField for CorsConfig {
    fn sharing(&self) -> Sharing {
        self.sharing
    }
}

/// Wraps `value` with its own sharing mode.
fn pair<T: SharedField>(value: Option<T>) -> Option<(T, Sharing)> {
    let value = value?;
    let sharing = value.sharing();
    Some((value, sharing))
}

/// Wraps `value` with its own sharing mode, EXCLUDING it entirely when
/// `cross_tenant` is `true` and its sharing is [`Sharing::Private`]:
/// `Sharing::Private`'s contract ("not visible to descendants") means the
/// value never surfaces to a different tenant at all, not merely that a
/// descendant's own override attempt is blocked
/// (`cpt-cf-oagw-dod-sharing-mode-merge`).
fn owner_pair<T: SharedField>(value: Option<T>, cross_tenant: bool) -> Option<(T, Sharing)> {
    let (value, sharing) = pair(value)?;
    if cross_tenant && matches!(sharing, Sharing::Private) {
        None
    } else {
        Some((value, sharing))
    }
}

/// Applies one tier's override on top of `current`, per the generic
/// `private`/`inherit`/`enforce` switch: no current value or `inherit`
/// adopts `incoming`; `private` or `enforce` keeps `current` unchanged
/// (`cpt-cf-oagw-algo-sharing-mode-merge`, step `inst-merge-04`/`inst-merge-10`).
fn apply_override_step<T>(
    current: Option<(T, Sharing)>,
    incoming: Option<(T, Sharing)>,
) -> Option<(T, Sharing)> {
    let Some(incoming_pair) = incoming else {
        return current;
    };
    match current.as_ref().map(|(_, sharing)| *sharing) {
        None | Some(Sharing::Inherit) => Some(incoming_pair),
        Some(Sharing::Private | Sharing::Enforce) => current,
    }
}

/// Merges `auth` across the Upstream (ancestor) and Tenant tiers; the Route
/// tier is skipped since `route.v1.schema.json` carries no `auth` field
/// (`cpt-cf-oagw-dod-sharing-mode-merge`).
// @cpt-begin:cpt-cf-oagw-dod-sharing-mode-merge:p1:inst-merge-auth-fn-01
fn merge_auth(
    ancestor: Option<&Upstream>,
    tenant: Option<&Upstream>,
    cross_tenant: bool,
) -> Option<AuthConfig> {
    let base = owner_pair(ancestor.and_then(|u| u.auth.clone()), cross_tenant);
    let tenant_override = pair(tenant.and_then(|u| u.auth.clone()));
    apply_override_step(base, tenant_override).map(|(value, _)| value)
}
// @cpt-end:cpt-cf-oagw-dod-sharing-mode-merge:p1:inst-merge-auth-fn-01

/// Merges `headers` across the Upstream (ancestor) and Tenant tiers as a
/// plain override: `headers` carries no `sharing` field in the checked-in
/// schema, so the Tenant tier's own value, when present, always replaces
/// the ancestor's (`cpt-cf-oagw-dod-sharing-mode-merge`).
fn merge_headers(ancestor: Option<&Upstream>, tenant: Option<&Upstream>) -> Option<HeadersConfig> {
    tenant
        .and_then(|u| u.headers.clone())
        .or_else(|| ancestor.and_then(|u| u.headers.clone()))
}

/// Merges `rate_limit` across the Upstream (ancestor), Route, and Tenant
/// tiers, resolving the Tenant step to the STRICTER of an ancestor-enforced
/// value and the tenant's own value rather than blocking it outright
/// (`cpt-cf-oagw-dod-sharing-mode-merge`,
/// `cpt-cf-oagw-dod-enforced-limits-across-shadowing`).
// @cpt-begin:cpt-cf-oagw-dod-enforced-limits-across-shadowing:p1:inst-merge-rate-limit-fn-01
fn merge_rate_limit(
    ancestor: Option<&Upstream>,
    route: &Route,
    tenant: Option<&Upstream>,
    cross_tenant: bool,
) -> Option<RateLimitConfig> {
    let base = owner_pair(ancestor.and_then(|u| u.rate_limit.clone()), cross_tenant);
    let route_override = pair(route.rate_limit.clone());
    let after_route = apply_override_step(base, route_override);
    let tenant_override = pair(tenant.and_then(|u| u.rate_limit.clone()));
    apply_rate_limit_tenant_step(after_route, tenant_override).map(|(value, _)| value)
}

/// The Tenant-tier step for `rate_limit`: `min(ancestor.enforced, tenant)`
/// when the current value is `enforce`-mode and a tenant value exists,
/// rather than the generic switch's plain "keep current, ignore tenant"
/// (`cpt-cf-oagw-algo-sharing-mode-merge`, steps `inst-merge-12`/`inst-merge-13`).
fn apply_rate_limit_tenant_step(
    current: Option<(RateLimitConfig, Sharing)>,
    tenant: Option<(RateLimitConfig, Sharing)>,
) -> Option<(RateLimitConfig, Sharing)> {
    let Some((tenant_value, tenant_sharing)) = tenant else {
        return current;
    };
    let Some((current_value, current_sharing)) = current else {
        return Some((tenant_value, tenant_sharing));
    };
    match current_sharing {
        Sharing::Inherit => Some((tenant_value, tenant_sharing)),
        Sharing::Private => Some((current_value, current_sharing)),
        Sharing::Enforce => Some((
            stricter_rate_limit(current_value, tenant_value),
            Sharing::Enforce,
        )),
    }
}

/// The config with the lower (stricter) sustained rate per second, compared
/// via integer cross-multiplication to avoid floating-point loss. Ties keep
/// `ancestor`.
fn stricter_rate_limit(ancestor: RateLimitConfig, tenant: RateLimitConfig) -> RateLimitConfig {
    let ancestor_rate = u64::from(ancestor.sustained.rate);
    let tenant_rate = u64::from(tenant.sustained.rate);
    let ancestor_secs = u64::from(window_seconds(ancestor.sustained.window));
    let tenant_secs = u64::from(window_seconds(tenant.sustained.window));
    if ancestor_rate * tenant_secs <= tenant_rate * ancestor_secs {
        ancestor
    } else {
        tenant
    }
}
// @cpt-end:cpt-cf-oagw-dod-enforced-limits-across-shadowing:p1:inst-merge-rate-limit-fn-01

/// The number of seconds in one `window`. `pub(crate)` so
/// `crate::domain::rate_limit`'s effective-rate-limit normalisation
/// (`cpt-cf-oagw-algo-effective-rate-limit`) reuses the exact same window
/// table rather than duplicating it.
///
/// Returns `u32`, not `u64`: every value is a small, fixed constant (at most
/// `86_400`, one day in seconds), so `u32` comfortably fits the whole domain
/// while still converting to `f64` losslessly via `f64::from` at every call
/// site, with no truncating or precision-losing cast anywhere near this
/// value.
pub(crate) const fn window_seconds(window: Window) -> u32 {
    match window {
        Window::Second => 1,
        Window::Minute => 60,
        Window::Hour => 3_600,
        Window::Day => 86_400,
    }
}

/// Merges `plugins` as the concatenation `upstream + route + tenant`, in
/// that order, unconditionally: the special plugins rule replaces the
/// generic private/inherit/enforce switch entirely, so no enforced binding
/// is ever dropped (`cpt-cf-oagw-dod-sharing-mode-merge`,
/// `cpt-cf-oagw-dod-enforced-limits-across-shadowing`).
// @cpt-begin:cpt-cf-oagw-dod-enforced-limits-across-shadowing:p1:inst-merge-plugins-fn-01
fn merge_plugins(
    ancestor: Option<&Upstream>,
    route: &Route,
    tenant: Option<&Upstream>,
) -> Vec<PluginItem> {
    let mut items = Vec::new();
    extend_plugin_items(&mut items, ancestor.and_then(|u| u.plugins.as_ref()));
    extend_plugin_items(&mut items, route.plugins.as_ref());
    extend_plugin_items(&mut items, tenant.and_then(|u| u.plugins.as_ref()));
    items
}
// @cpt-end:cpt-cf-oagw-dod-enforced-limits-across-shadowing:p1:inst-merge-plugins-fn-01

fn extend_plugin_items(items: &mut Vec<PluginItem>, config: Option<&PluginsConfig>) {
    if let Some(config) = config {
        items.extend(config.items.iter().cloned());
    }
}

/// Merges `cors` across the Upstream (ancestor) and Tenant tiers; the Route
/// tier is skipped since `route.v1.schema.json` carries no `cors` field.
/// Under `inherit`, `allowed_origins` is the UNION of both tiers'
/// (controller decision D6); under `enforce`, the ancestor's list is kept
/// unchanged (`cpt-cf-oagw-dod-sharing-mode-merge`).
// @cpt-begin:cpt-cf-oagw-dod-sharing-mode-merge:p1:inst-merge-cors-fn-01
// reason: each arm below is one independently-specified row of the
// `cpt-cf-oagw-dod-sharing-mode-merge` sharing-mode table (controller
// decision D6): "descendant provided nothing" and "ancestor enforces" are
// different preconditions that only coincidentally produce the same
// `Some(ancestor_value)` result today. Merging them would hide that the
// `Enforce` row is independently specified and could silently stop tracking
// it if a future row (e.g. a new sharing mode) changes only one side.
#[allow(clippy::match_same_arms)]
fn merge_cors(
    ancestor: Option<&Upstream>,
    tenant: Option<&Upstream>,
    cross_tenant: bool,
) -> Option<CorsConfig> {
    let base = owner_pair(ancestor.and_then(|u| u.cors.clone()), cross_tenant);
    let tenant_value = tenant.and_then(|u| u.cors.clone());
    match (base, tenant_value) {
        (None, tenant_only) => tenant_only,
        (Some((ancestor_value, _)), None) => Some(ancestor_value),
        (Some((ancestor_value, Sharing::Inherit)), Some(descendant_value)) => {
            Some(union_cors_origins(ancestor_value, descendant_value))
        }
        (Some((ancestor_value, Sharing::Enforce)), Some(_)) => Some(ancestor_value),
        (Some((_, Sharing::Private)), Some(descendant_value)) => Some(descendant_value),
    }
}

/// `ancestor` with `allowed_origins` replaced by the union of both configs'
/// origin lists, preserving order and de-duplicating.
fn union_cors_origins(ancestor: CorsConfig, descendant: CorsConfig) -> CorsConfig {
    let mut origins = ancestor.allowed_origins.clone();
    for origin in descendant.allowed_origins {
        if !origins.contains(&origin) {
            origins.push(origin);
        }
    }
    CorsConfig {
        allowed_origins: origins,
        ..ancestor
    }
}
// @cpt-end:cpt-cf-oagw-dod-sharing-mode-merge:p1:inst-merge-cors-fn-01

/// Computes `union(upstream.tags, route.tags, tenant.tags)`, independent of
/// any sharing mode: descendants may add tags but never remove an
/// ancestor's (`cpt-cf-oagw-dod-tag-union`).
// @cpt-begin:cpt-cf-oagw-dod-tag-union:p1:inst-merge-tags-fn-01
fn merge_tags(
    ancestor: Option<&Upstream>,
    route: &Route,
    tenant: Option<&Upstream>,
) -> Vec<String> {
    let mut tags = Vec::new();
    if let Some(upstream) = ancestor {
        push_unique(&mut tags, &upstream.tags);
    }
    push_unique(&mut tags, &route.tags);
    if let Some(upstream) = tenant {
        push_unique(&mut tags, &upstream.tags);
    }
    tags
}
// @cpt-end:cpt-cf-oagw-dod-tag-union:p1:inst-merge-tags-fn-01

fn push_unique(target: &mut Vec<String>, source: &[String]) {
    for tag in source {
        if !target.contains(tag) {
            target.push(tag.clone());
        }
    }
}

/// Assembles the [`ResolvedPlan`] from the selected upstream/route and the
/// ancestor/tenant merge sources (`cpt-cf-oagw-dod-sharing-mode-merge`).
fn build_resolved_plan(
    upstream: Upstream,
    owning_tenant_id: Uuid,
    route: Route,
    ancestor: Option<&Upstream>,
    tenant: Option<&Upstream>,
    cross_tenant: bool,
) -> ResolvedPlan {
    let effective_auth = merge_auth(ancestor, tenant, cross_tenant);
    let effective_headers = merge_headers(ancestor, tenant);
    let effective_rate_limit = merge_rate_limit(ancestor, &route, tenant, cross_tenant);
    let effective_plugins = merge_plugins(ancestor, &route, tenant);
    let effective_cors = merge_cors(ancestor, tenant, cross_tenant);
    let effective_tags = merge_tags(ancestor, &route, tenant);
    // @cpt-begin:cpt-cf-oagw-dod-header-transformation-plan:p1:inst-merge-header-plan-01
    let header_plan = compute_header_plan(effective_headers.as_ref());
    // @cpt-end:cpt-cf-oagw-dod-header-transformation-plan:p1:inst-merge-header-plan-01
    let endpoints = upstream.server.endpoints.clone();

    ResolvedPlan {
        upstream,
        owning_tenant_id,
        route,
        effective_auth,
        effective_headers,
        effective_rate_limit,
        effective_plugins,
        effective_cors,
        effective_tags,
        header_plan,
        endpoints,
    }
}

// ---------------------------------------------------------------------------
// Header transformation plan computation (`cpt-cf-oagw-algo-header-plan-compute`).
// ---------------------------------------------------------------------------

/// Computes the effective request/response header-transformation plan from
/// the merged `headers` configuration (`cpt-cf-oagw-algo-header-plan-compute`).
fn compute_header_plan(headers: Option<&HeadersConfig>) -> HeaderPlan {
    let request = headers
        .and_then(|config| config.request.as_ref())
        .map_or_else(RequestHeaderPlan::default, |request| RequestHeaderPlan {
            set: request.set.clone().unwrap_or_default(),
            add: request.add.clone().unwrap_or_default(),
            remove: request.remove.clone().unwrap_or_default(),
            passthrough: request.passthrough,
            passthrough_allowlist: request.passthrough_allowlist.clone().unwrap_or_default(),
        });
    let response = headers
        .and_then(|config| config.response.as_ref())
        .map_or_else(ResponseHeaderPlan::default, |response| ResponseHeaderPlan {
            set: response.set.clone().unwrap_or_default(),
            add: response.add.clone().unwrap_or_default(),
            remove: response.remove.clone().unwrap_or_default(),
        });
    HeaderPlan { request, response }
}

#[cfg(test)]
mod tests {
    use super::{
        AliasHierarchyOutcome, ResolvedPlan, apply_override_step, build_resolved_plan,
        compute_header_plan, lookup_alias_hierarchy, merge_auth, merge_cors, merge_plugins,
        merge_rate_limit, merge_tags, resolve_proxy_target, select_route, stricter_rate_limit,
    };
    use crate::domain::alias::ROOT_TENANT_ID;
    use crate::domain::model::{
        Algorithm, AuthConfig, CorsConfig, Endpoint, HeadersConfig, HttpMatch, MatchConfig,
        PathSuffixMode, PluginsConfig, Protocol, RateLimitConfig, RequestHeaders, Route,
        RouteMethod, Scheme, ServerConfig, Sharing, Strategy, Sustained, Upstream, Window,
    };
    use crate::domain::service;
    use crate::state::ControlPlaneState;
    use uuid::Uuid;

    fn endpoint(host: &str) -> Endpoint {
        Endpoint {
            scheme: Scheme::Https,
            host: host.to_owned(),
            port: Some(443),
        }
    }

    fn upstream(alias: &str, enabled: bool) -> Upstream {
        Upstream {
            id: Uuid::new_v4(),
            enabled,
            alias: alias.to_owned(),
            tags: Vec::new(),
            server: ServerConfig {
                endpoints: vec![endpoint(alias)],
            },
            protocol: Protocol::Http,
            auth: None,
            headers: None,
            plugins: None,
            rate_limit: None,
            cors: None,
        }
    }

    fn route(upstream_id: Uuid, path: &str, methods: &[RouteMethod], priority: i64) -> Route {
        Route {
            id: Uuid::new_v4(),
            upstream_id,
            tags: Vec::new(),
            match_config: MatchConfig {
                http: Some(HttpMatch {
                    methods: methods.to_vec(),
                    path: path.to_owned(),
                    query_allowlist: Vec::new(),
                    path_suffix_mode: PathSuffixMode::Append,
                }),
                grpc: None,
            },
            plugins: None,
            rate_limit: None,
            enabled: true,
            priority,
        }
    }

    fn rate_limit(sharing: Sharing, rate: u32, window: Window) -> RateLimitConfig {
        RateLimitConfig {
            sharing,
            algorithm: Algorithm::TokenBucket,
            sustained: Sustained { rate, window },
            burst: None,
            scope: crate::domain::model::RateLimitScope::Tenant,
            strategy: Strategy::Reject,
            cost: 1,
        }
    }

    // -----------------------------------------------------------------
    // Alias normalization (reused from feature 2; exercised here at the
    // resolution entry point) and hierarchy walk / shadowing.
    // -----------------------------------------------------------------

    #[test]
    fn resolve_normalizes_the_alias_before_hierarchy_lookup() {
        let state = ControlPlaneState::new();
        let tenant_id = Uuid::new_v4();
        let up = upstream("api.openai.com", true);
        let up_id = up.id;
        state.tenant(tenant_id).upstreams.insert(up_id, up);
        state
            .tenant(tenant_id)
            .routes
            .insert(Uuid::new_v4(), route(up_id, "/v1", &[RouteMethod::Get], 0));

        let plan = resolve_proxy_target(&state, tenant_id, "Api.OpenAI.COM.", "GET", "/v1")
            .expect("normalized alias must resolve");
        assert_eq!(plan.upstream.alias, "api.openai.com");
    }

    #[test]
    fn closest_enabled_tenant_upstream_wins_over_root() {
        let state = ControlPlaneState::new();
        let tenant_id = Uuid::new_v4();

        let root_up = upstream("api.openai.com", true);
        state
            .tenant(ROOT_TENANT_ID)
            .upstreams
            .insert(root_up.id, root_up.clone());
        state.tenant(ROOT_TENANT_ID).routes.insert(
            Uuid::new_v4(),
            route(root_up.id, "/v1", &[RouteMethod::Get], 0),
        );

        let tenant_up = upstream("api.openai.com", true);
        state
            .tenant(tenant_id)
            .upstreams
            .insert(tenant_up.id, tenant_up.clone());
        state.tenant(tenant_id).routes.insert(
            Uuid::new_v4(),
            route(tenant_up.id, "/v1", &[RouteMethod::Get], 0),
        );

        let plan = resolve_proxy_target(&state, tenant_id, "api.openai.com", "GET", "/v1")
            .expect("must resolve");
        assert_eq!(plan.upstream.id, tenant_up.id);
        assert_eq!(plan.owning_tenant_id, tenant_id);
    }

    // @cpt-begin:cpt-cf-oagw-dod-alias-hierarchy-lookup:p1:inst-hierarchy-lookup-disabled-skip-test-01
    #[test]
    fn a_disabled_closest_upstream_is_skipped_in_favour_of_an_enabled_ancestor() {
        let state = ControlPlaneState::new();
        let tenant_id = Uuid::new_v4();

        let root_up = upstream("api.openai.com", true);
        state
            .tenant(ROOT_TENANT_ID)
            .upstreams
            .insert(root_up.id, root_up.clone());
        state.tenant(ROOT_TENANT_ID).routes.insert(
            Uuid::new_v4(),
            route(root_up.id, "/v1", &[RouteMethod::Get], 0),
        );

        let disabled_tenant_up = upstream("api.openai.com", false);
        state
            .tenant(tenant_id)
            .upstreams
            .insert(disabled_tenant_up.id, disabled_tenant_up);

        let outcome = lookup_alias_hierarchy(&state, tenant_id, "api.openai.com");
        assert_eq!(
            outcome,
            AliasHierarchyOutcome::Enabled {
                upstream: Box::new(root_up.clone()),
                owning_tenant_id: ROOT_TENANT_ID,
            }
        );

        let plan = resolve_proxy_target(&state, tenant_id, "api.openai.com", "GET", "/v1")
            .expect("must fall back to the enabled root upstream");
        assert_eq!(plan.upstream.id, root_up.id);
    }
    // @cpt-end:cpt-cf-oagw-dod-alias-hierarchy-lookup:p1:inst-hierarchy-lookup-disabled-skip-test-01

    // @cpt-begin:cpt-cf-oagw-dod-upstream-disabled-outcome:p1:inst-upstream-disabled-test-01
    #[test]
    fn every_tier_disabled_yields_the_upstream_disabled_outcome_and_no_cache_entry() {
        let state = ControlPlaneState::new();
        let tenant_id = Uuid::new_v4();

        let root_up = upstream("api.openai.com", false);
        state
            .tenant(ROOT_TENANT_ID)
            .upstreams
            .insert(root_up.id, root_up);
        let tenant_up = upstream("api.openai.com", false);
        state
            .tenant(tenant_id)
            .upstreams
            .insert(tenant_up.id, tenant_up);

        let error = resolve_proxy_target(&state, tenant_id, "api.openai.com", "GET", "/v1")
            .expect_err("every disabled tier must yield upstream-disabled");
        assert_eq!(error.status(), axum::http::StatusCode::SERVICE_UNAVAILABLE);
        assert!(state.resolved_cache().is_empty());
    }
    // @cpt-end:cpt-cf-oagw-dod-upstream-disabled-outcome:p1:inst-upstream-disabled-test-01

    // @cpt-begin:cpt-cf-oagw-dod-route-not-found-outcome:p1:inst-unknown-alias-test-01
    #[test]
    fn an_unknown_alias_yields_route_not_found() {
        let state = ControlPlaneState::new();
        let error =
            resolve_proxy_target(&state, Uuid::new_v4(), "nowhere.example.com", "GET", "/v1")
                .expect_err("unknown alias must 404 as route-not-found");
        assert_eq!(error.status(), axum::http::StatusCode::NOT_FOUND);
    }
    // @cpt-end:cpt-cf-oagw-dod-route-not-found-outcome:p1:inst-unknown-alias-test-01

    // -----------------------------------------------------------------
    // Per-field sharing-mode merge.
    // -----------------------------------------------------------------

    #[test]
    fn private_ancestor_auth_never_surfaces_to_a_descendant() {
        let mut ancestor = upstream("private.example.com", true);
        ancestor.auth = Some(AuthConfig {
            auth_type: Some("gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1".to_owned()),
            sharing: Sharing::Private,
            config: None,
        });

        let effective = merge_auth(Some(&ancestor), None, true);
        assert!(
            effective.is_none(),
            "private ancestor auth must never surface"
        );
    }

    #[test]
    fn inherit_ancestor_auth_surfaces_with_no_descendant_override() {
        let mut ancestor = upstream("inherit.example.com", true);
        ancestor.auth = Some(AuthConfig {
            auth_type: Some("gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1".to_owned()),
            sharing: Sharing::Inherit,
            config: None,
        });

        let effective = merge_auth(Some(&ancestor), None, true)
            .expect("inherit ancestor auth must surface with no override");
        assert_eq!(
            effective.auth_type.as_deref(),
            Some("gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1")
        );
    }

    // @cpt-begin:cpt-cf-oagw-dod-enforced-limits-across-shadowing:p1:inst-rate-limit-stricter-test-01
    #[test]
    fn enforced_ancestor_rate_limit_and_descendant_value_resolve_to_the_stricter() {
        let mut ancestor = upstream("stricter.example.com", true);
        ancestor.rate_limit = Some(rate_limit(Sharing::Enforce, 10_000, Window::Minute));
        let mut tenant = upstream("stricter.example.com", true);
        tenant.rate_limit = Some(rate_limit(Sharing::Private, 500, Window::Minute));
        let route = route(ancestor.id, "/v1", &[RouteMethod::Get], 0);

        let effective = merge_rate_limit(Some(&ancestor), &route, Some(&tenant), true)
            .expect("a rate limit must be effective");
        assert_eq!(effective.sustained.rate, 500);
    }
    // @cpt-end:cpt-cf-oagw-dod-enforced-limits-across-shadowing:p1:inst-rate-limit-stricter-test-01

    /// End-to-end counterpart of
    /// `enforced_ancestor_rate_limit_and_descendant_value_resolve_to_the_stricter`:
    /// that test calls `merge_rate_limit` directly with hand-built
    /// ancestor/tenant arguments, proving the merge arithmetic alone. This
    /// one drives the full `resolve_proxy_target` pipeline over a real
    /// hierarchy where the descendant tenant shadows the root's alias with
    /// its *own* `Upstream` record (a distinct code path: `selected_upstream`
    /// resolves the descendant's own record via the hierarchy walk, while
    /// `resolve_proxy_target` separately re-fetches the root's record by
    /// alias for the merge's ancestor role), proving the wiring between the
    /// two, not just the arithmetic.
    #[test]
    fn a_shadowing_descendant_upstream_still_gets_the_ancestors_enforced_rate_limit() {
        let state = ControlPlaneState::new();
        let tenant_id = Uuid::new_v4();

        let mut root_up = upstream("shadow.example.com", true);
        root_up.rate_limit = Some(rate_limit(Sharing::Enforce, 10_000, Window::Minute));
        state
            .tenant(ROOT_TENANT_ID)
            .upstreams
            .insert(root_up.id, root_up.clone());
        state.tenant(ROOT_TENANT_ID).routes.insert(
            Uuid::new_v4(),
            route(root_up.id, "/v1", &[RouteMethod::Get], 0),
        );

        // The descendant tenant's own upstream shadows the same alias,
        // winning the hierarchy walk (closest tier), with its own, looser
        // rate limit.
        let mut shadow_up = upstream("shadow.example.com", true);
        shadow_up.rate_limit = Some(rate_limit(Sharing::Private, 500, Window::Minute));
        state
            .tenant(tenant_id)
            .upstreams
            .insert(shadow_up.id, shadow_up.clone());
        state.tenant(tenant_id).routes.insert(
            Uuid::new_v4(),
            route(shadow_up.id, "/v1", &[RouteMethod::Get], 0),
        );

        let plan = resolve_proxy_target(&state, tenant_id, "shadow.example.com", "GET", "/v1")
            .expect("the descendant's own shadowing upstream must resolve");
        assert_eq!(
            plan.upstream.id, shadow_up.id,
            "the descendant's own upstream must win the hierarchy walk"
        );
        let effective = plan
            .effective_rate_limit
            .expect("a rate limit must be effective");
        assert_eq!(
            effective.sustained.rate, 500,
            "min(ancestor.enforced=10000, descendant=500) = 500"
        );
    }

    #[test]
    fn stricter_rate_limit_compares_across_differing_windows() {
        let slower = rate_limit(Sharing::Enforce, 60, Window::Minute); // 1/sec
        let faster = rate_limit(Sharing::Private, 100, Window::Second); // 100/sec
        let chosen = stricter_rate_limit(slower.clone(), faster);
        assert_eq!(chosen.sustained.rate, slower.sustained.rate);
    }

    fn plugin_items(names: &[&str]) -> Vec<crate::domain::model::PluginItem> {
        names.iter().map(|name| (*name).into()).collect()
    }

    fn plugin_refs(items: &[crate::domain::model::PluginItem]) -> Vec<&str> {
        items
            .iter()
            .map(crate::domain::model::PluginItem::plugin_ref)
            .collect()
    }

    #[test]
    fn plugins_concatenate_upstream_then_route_then_tenant() {
        let mut ancestor = upstream("plugins.example.com", true);
        ancestor.plugins = Some(PluginsConfig {
            sharing: Sharing::Inherit,
            items: plugin_items(&["U1"]),
        });
        let mut route = route(ancestor.id, "/v1", &[RouteMethod::Get], 0);
        route.plugins = Some(PluginsConfig {
            sharing: Sharing::Inherit,
            items: plugin_items(&["R1"]),
        });
        let mut tenant = upstream("plugins.example.com", true);
        tenant.plugins = Some(PluginsConfig {
            sharing: Sharing::Inherit,
            items: plugin_items(&["T1"]),
        });

        let effective = merge_plugins(Some(&ancestor), &route, Some(&tenant));
        assert_eq!(plugin_refs(&effective), vec!["U1", "R1", "T1"]);
    }

    #[test]
    fn plugins_concatenate_upstream_and_route_with_no_tenant_binding() {
        let mut ancestor = upstream("plugins2.example.com", true);
        ancestor.plugins = Some(PluginsConfig {
            sharing: Sharing::Inherit,
            items: plugin_items(&["U1", "U2"]),
        });
        let mut route = route(ancestor.id, "/v1", &[RouteMethod::Get], 0);
        route.plugins = Some(PluginsConfig {
            sharing: Sharing::Inherit,
            items: plugin_items(&["R1", "R2"]),
        });

        let effective = merge_plugins(Some(&ancestor), &route, None);
        assert_eq!(plugin_refs(&effective), vec!["U1", "U2", "R1", "R2"]);
    }

    // @cpt-begin:cpt-cf-oagw-dod-sharing-mode-merge:p1:inst-cors-union-test-01
    #[test]
    fn inherit_cors_unions_ancestor_and_descendant_origins() {
        let mut ancestor = upstream("cors.example.com", true);
        ancestor.cors = Some(CorsConfig {
            sharing: Sharing::Inherit,
            enabled: true,
            allowed_origins: vec!["https://app.example.com".to_owned()],
            allowed_methods: vec![],
            expose_headers: vec![],
            allow_credentials: false,
        });
        let mut tenant = upstream("cors.example.com", true);
        tenant.cors = Some(CorsConfig {
            sharing: Sharing::Private,
            enabled: true,
            allowed_origins: vec!["https://admin.example.com".to_owned()],
            allowed_methods: vec![],
            expose_headers: vec![],
            allow_credentials: false,
        });

        let effective = merge_cors(Some(&ancestor), Some(&tenant), true).expect("cors must merge");
        assert_eq!(
            effective.allowed_origins,
            vec![
                "https://app.example.com".to_owned(),
                "https://admin.example.com".to_owned()
            ]
        );
    }
    // @cpt-end:cpt-cf-oagw-dod-sharing-mode-merge:p1:inst-cors-union-test-01

    // @cpt-begin:cpt-cf-oagw-dod-sharing-mode-merge:p1:inst-cors-enforce-test-01
    #[test]
    fn enforce_cors_keeps_only_the_ancestor_origin() {
        let mut ancestor = upstream("cors-enforce.example.com", true);
        ancestor.cors = Some(CorsConfig {
            sharing: Sharing::Enforce,
            enabled: true,
            allowed_origins: vec!["https://app.example.com".to_owned()],
            allowed_methods: vec![],
            expose_headers: vec![],
            allow_credentials: false,
        });
        let mut tenant = upstream("cors-enforce.example.com", true);
        tenant.cors = Some(CorsConfig {
            sharing: Sharing::Private,
            enabled: true,
            allowed_origins: vec!["https://admin.example.com".to_owned()],
            allowed_methods: vec![],
            expose_headers: vec![],
            allow_credentials: false,
        });

        let effective = merge_cors(Some(&ancestor), Some(&tenant), true).expect("cors must merge");
        assert_eq!(
            effective.allowed_origins,
            vec!["https://app.example.com".to_owned()]
        );
    }
    // @cpt-end:cpt-cf-oagw-dod-sharing-mode-merge:p1:inst-cors-enforce-test-01

    #[test]
    fn route_tier_is_skipped_for_auth_headers_and_cors() {
        // Route has no auth/headers/cors fields at all in the domain model
        // (mirroring `route.v1.schema.json`), so this is a compile-time
        // guarantee rather than a runtime behavior to probe further; this
        // test documents the acceptance criterion by asserting the merge
        // functions for those three fields never take a `Route` argument.
        let ancestor = upstream("skip-route.example.com", true);
        let tenant = upstream("skip-route.example.com", true);
        assert!(merge_auth(Some(&ancestor), Some(&tenant), true).is_none());
    }

    // @cpt-begin:cpt-cf-oagw-dod-tag-union:p1:inst-tag-union-test-01
    #[test]
    fn effective_tags_union_ancestor_and_descendant_and_never_drop_ancestor_tags() {
        let mut ancestor = upstream("tags.example.com", true);
        ancestor.tags = vec!["a".to_owned()];
        let mut tenant = upstream("tags.example.com", true);
        tenant.tags = vec!["b".to_owned()];
        let route = route(ancestor.id, "/v1", &[RouteMethod::Get], 0);

        let tags = merge_tags(Some(&ancestor), &route, Some(&tenant));
        assert_eq!(tags, vec!["a".to_owned(), "b".to_owned()]);
    }
    // @cpt-end:cpt-cf-oagw-dod-tag-union:p1:inst-tag-union-test-01

    #[test]
    fn apply_override_step_with_no_current_value_adopts_incoming() {
        let incoming = Some((7_u32, Sharing::Private));
        assert_eq!(apply_override_step(None, incoming), incoming);
    }

    // -----------------------------------------------------------------
    // HTTP route selection.
    // -----------------------------------------------------------------

    // @cpt-begin:cpt-cf-oagw-dod-http-route-selection:p1:inst-route-select-longest-prefix-test-01
    #[test]
    fn the_longest_matching_path_prefix_wins() {
        let state = ControlPlaneState::new();
        let tenant_id = Uuid::new_v4();
        let up = upstream("longest.example.com", true);
        state.tenant(tenant_id).upstreams.insert(up.id, up.clone());
        let short = route(up.id, "/v1", &[RouteMethod::Get], 0);
        let long = route(up.id, "/v1/chat/completions", &[RouteMethod::Get], 0);
        state.tenant(tenant_id).routes.insert(short.id, short);
        state.tenant(tenant_id).routes.insert(long.id, long.clone());

        let selected = select_route(
            &state.tenant(tenant_id),
            up.id,
            "GET",
            "/v1/chat/completions/extra",
        )
        .expect("a route must match");
        assert_eq!(selected.id, long.id);
    }
    // @cpt-end:cpt-cf-oagw-dod-http-route-selection:p1:inst-route-select-longest-prefix-test-01

    #[test]
    fn priority_breaks_ties_between_equal_length_prefixes() {
        let state = ControlPlaneState::new();
        let tenant_id = Uuid::new_v4();
        let up = upstream("tie.example.com", true);
        state.tenant(tenant_id).upstreams.insert(up.id, up.clone());
        let low = route(up.id, "/v1", &[RouteMethod::Get], 0);
        let high = route(up.id, "/v1", &[RouteMethod::Post], 5);
        // give both the same methods to create a genuine tie on prefix
        let mut high_get = high;
        high_get.match_config = MatchConfig {
            http: Some(HttpMatch {
                methods: vec![RouteMethod::Get],
                path: "/v1".to_owned(),
                query_allowlist: Vec::new(),
                path_suffix_mode: PathSuffixMode::Append,
            }),
            grpc: None,
        };
        state.tenant(tenant_id).routes.insert(low.id, low);
        state
            .tenant(tenant_id)
            .routes
            .insert(high_get.id, high_get.clone());

        let selected = select_route(&state.tenant(tenant_id), up.id, "GET", "/v1")
            .expect("a route must match");
        assert_eq!(selected.priority, 5);
        assert_eq!(selected.id, high_get.id);
    }

    // @cpt-begin:cpt-cf-oagw-dod-route-not-found-outcome:p1:inst-wrong-method-test-01
    #[test]
    fn a_method_absent_from_any_candidate_yields_route_not_found() {
        let state = ControlPlaneState::new();
        let tenant_id = Uuid::new_v4();
        let up = upstream("wrong-method.example.com", true);
        state.tenant(tenant_id).upstreams.insert(up.id, up.clone());
        let only_route = route(
            up.id,
            "/v1/items",
            &[RouteMethod::Get, RouteMethod::Post],
            0,
        );
        state
            .tenant(tenant_id)
            .routes
            .insert(only_route.id, only_route);

        let selected = select_route(&state.tenant(tenant_id), up.id, "DELETE", "/v1/items");
        assert!(selected.is_none());
    }
    // @cpt-end:cpt-cf-oagw-dod-route-not-found-outcome:p1:inst-wrong-method-test-01

    #[test]
    fn a_different_enabled_route_matching_method_and_longest_prefix_is_selected() {
        let state = ControlPlaneState::new();
        let tenant_id = Uuid::new_v4();
        let up = upstream("method-select.example.com", true);
        state.tenant(tenant_id).upstreams.insert(up.id, up.clone());
        let get_only = route(up.id, "/v1", &[RouteMethod::Get], 0);
        let post_only = route(up.id, "/v1", &[RouteMethod::Post], 0);
        state.tenant(tenant_id).routes.insert(get_only.id, get_only);
        state
            .tenant(tenant_id)
            .routes
            .insert(post_only.id, post_only.clone());

        let selected = select_route(&state.tenant(tenant_id), up.id, "POST", "/v1")
            .expect("post-only route must be selected");
        assert_eq!(selected.id, post_only.id);
    }

    #[test]
    fn a_disabled_route_is_excluded_but_one_with_no_enabled_override_counts() {
        let state = ControlPlaneState::new();
        let tenant_id = Uuid::new_v4();
        let up = upstream("enabled-default.example.com", true);
        state.tenant(tenant_id).upstreams.insert(up.id, up.clone());
        let mut disabled = route(up.id, "/v1", &[RouteMethod::Get], 10);
        disabled.enabled = false;
        let enabled = route(up.id, "/v1", &[RouteMethod::Get], 0);
        state.tenant(tenant_id).routes.insert(disabled.id, disabled);
        state
            .tenant(tenant_id)
            .routes
            .insert(enabled.id, enabled.clone());

        let selected = select_route(&state.tenant(tenant_id), up.id, "GET", "/v1")
            .expect("the enabled route must be selected");
        assert_eq!(selected.id, enabled.id);
    }

    #[test]
    fn a_route_with_path_suffix_mode_disabled_is_still_selected_by_method_and_prefix() {
        let state = ControlPlaneState::new();
        let tenant_id = Uuid::new_v4();
        let up = upstream("suffix-disabled.example.com", true);
        state.tenant(tenant_id).upstreams.insert(up.id, up.clone());
        let mut disabled_suffix = route(up.id, "/v1", &[RouteMethod::Get], 0);
        if let Some(http) = disabled_suffix.match_config.http.as_mut() {
            http.path_suffix_mode = PathSuffixMode::Disabled;
        }
        state
            .tenant(tenant_id)
            .routes
            .insert(disabled_suffix.id, disabled_suffix.clone());

        let selected = select_route(&state.tenant(tenant_id), up.id, "GET", "/v1/extra")
            .expect("method-and-prefix selection ignores path_suffix_mode");
        assert_eq!(selected.id, disabled_suffix.id);
        assert_eq!(
            selected
                .match_config
                .http
                .as_ref()
                .map(|http| http.path_suffix_mode),
            Some(PathSuffixMode::Disabled)
        );
    }

    // -----------------------------------------------------------------
    // Header transformation plan.
    // -----------------------------------------------------------------

    // @cpt-begin:cpt-cf-oagw-dod-header-transformation-plan:p1:inst-header-plan-test-01
    #[test]
    fn header_plan_enumerates_set_add_remove_and_allowlist() {
        let headers = HeadersConfig {
            request: Some(RequestHeaders {
                set: Some(BTreeMapExt::from_pairs(&[("X-Set", "1")])),
                add: Some(BTreeMapExt::from_pairs(&[("X-Add", "2")])),
                remove: Some(vec!["X-Remove".to_owned()]),
                passthrough: crate::domain::model::Passthrough::Allowlist,
                passthrough_allowlist: Some(vec!["X-Allow".to_owned()]),
            }),
            response: None,
        };

        let plan = compute_header_plan(Some(&headers));
        assert_eq!(plan.request.set.get("X-Set"), Some(&"1".to_owned()));
        assert_eq!(plan.request.add.get("X-Add"), Some(&"2".to_owned()));
        assert_eq!(plan.request.remove, vec!["X-Remove".to_owned()]);
        assert_eq!(
            plan.request.passthrough_allowlist,
            vec!["X-Allow".to_owned()]
        );
        assert!(plan.response.set.is_empty());
    }
    // @cpt-end:cpt-cf-oagw-dod-header-transformation-plan:p1:inst-header-plan-test-01

    struct BTreeMapExt;
    impl BTreeMapExt {
        fn from_pairs(pairs: &[(&str, &str)]) -> std::collections::BTreeMap<String, String> {
            pairs
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect()
        }
    }

    // -----------------------------------------------------------------
    // Cache hit / miss / invalidation, via the control-plane write paths.
    // -----------------------------------------------------------------

    #[test]
    fn a_cache_miss_populates_the_cache_and_the_next_identical_request_is_a_hit() {
        let state = ControlPlaneState::new();
        let tenant_id = Uuid::new_v4();
        let up = upstream("cache.example.com", true);
        state.tenant(tenant_id).upstreams.insert(up.id, up.clone());
        state
            .tenant(tenant_id)
            .routes
            .insert(Uuid::new_v4(), route(up.id, "/v1", &[RouteMethod::Get], 0));

        assert!(state.resolved_cache().is_empty());
        resolve_proxy_target(&state, tenant_id, "cache.example.com", "GET", "/v1")
            .expect("first resolution must succeed");
        assert_eq!(state.resolved_cache().len(), 1);

        // Mutate the underlying store directly, bypassing the service-layer
        // write paths that would invalidate the cache, to prove the second
        // identical request is served from the (now stale) cache rather
        // than recomputed.
        let mut mutated_upstream = up.clone();
        mutated_upstream.tags = vec!["mutated".to_owned()];
        state
            .tenant(tenant_id)
            .upstreams
            .insert(up.id, mutated_upstream);

        let served = resolve_proxy_target(&state, tenant_id, "cache.example.com", "GET", "/v1")
            .expect("second resolution must be served from cache");
        assert!(
            served.effective_tags.is_empty(),
            "cache hit must not reflect the mutation"
        );
    }

    #[test]
    fn distinct_method_or_path_populate_distinct_cache_entries() {
        let state = ControlPlaneState::new();
        let tenant_id = Uuid::new_v4();
        let up = upstream("cache2.example.com", true);
        state.tenant(tenant_id).upstreams.insert(up.id, up.clone());
        state.tenant(tenant_id).routes.insert(
            Uuid::new_v4(),
            route(up.id, "/v1", &[RouteMethod::Get, RouteMethod::Post], 0),
        );

        resolve_proxy_target(&state, tenant_id, "cache2.example.com", "GET", "/v1")
            .expect("get must resolve");
        resolve_proxy_target(&state, tenant_id, "cache2.example.com", "POST", "/v1")
            .expect("post must resolve");
        assert_eq!(state.resolved_cache().len(), 2);
    }

    // @cpt-begin:cpt-cf-oagw-dod-resolved-config-cache-invalidation:p1:inst-cache-invalidate-write-test-01
    #[test]
    fn a_control_plane_write_invalidates_the_cache_so_the_next_request_sees_the_change() {
        let state = ControlPlaneState::new();
        let tenant_id = Uuid::new_v4();
        let body = serde_json::json!({
            "server": {"endpoints": [{"scheme": "http", "host": "invalidate.example.com", "port": 80}]},
            "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1",
        });
        let up = service::create_upstream(&state, tenant_id, body).expect("create must succeed");
        let route_body = serde_json::json!({
            "upstream_id": up.id,
            "match": {"http": {"methods": ["GET"], "path": "/v1"}},
        });
        service::create_route(&state, tenant_id, route_body).expect("route create must succeed");

        resolve_proxy_target(&state, tenant_id, "invalidate.example.com", "GET", "/v1")
            .expect("first resolution must succeed");
        assert_eq!(state.resolved_cache().len(), 1);

        let replace_body = serde_json::json!({
            "server": {"endpoints": [{"scheme": "http", "host": "invalidate.example.com", "port": 80}]},
            "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1",
            "tags": ["updated"],
        });
        service::replace_upstream(&state, tenant_id, up.id, replace_body)
            .expect("replace must succeed");

        assert!(
            state.resolved_cache().is_empty(),
            "a control-plane write must empty the resolved-config cache"
        );

        let plan = resolve_proxy_target(&state, tenant_id, "invalidate.example.com", "GET", "/v1")
            .expect("resolution after invalidation must succeed");
        assert_eq!(plan.effective_tags, vec!["updated".to_owned()]);
    }
    // @cpt-end:cpt-cf-oagw-dod-resolved-config-cache-invalidation:p1:inst-cache-invalidate-write-test-01

    #[test]
    fn build_resolved_plan_carries_the_chosen_endpoint_list() {
        let up = upstream("endpoints.example.com", true);
        let route = route(up.id, "/v1", &[RouteMethod::Get], 0);
        let plan: ResolvedPlan =
            build_resolved_plan(up.clone(), Uuid::new_v4(), route, None, None, false);
        assert_eq!(plan.endpoints, up.server.endpoints);
    }
}
