//! Alias, route and tenant-hierarchy resolution for the data plane.
//!
//! Resolution is the first pipeline stage: it turns `(tenant chain, alias)`
//! into the upstream that will actually be dialled plus the *merged*
//! configuration that governs the exchange.
//!
//! Tenant layering (PRD "configuration layering", `Upstream < Route < Tenant`):
//! a descendant's definition of an alias shadows its ancestors'; an ancestor's
//! block is visible to descendants only when it declares
//! `sharing: inherit` (overridable) or `sharing: enforce` (not overridable).
//! Rate limits merge as `min(parent, child)` (ADR 0003); an ancestor's
//! `enforce` block wins outright.

use std::collections::BTreeSet;
use std::sync::Arc;

use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::domain::model::upstream::normalize_host;
use crate::domain::model::{
    AuthBinding, CorsConfig, Endpoint, HeaderRules, PluginBinding, PluginChain, RateLimitConfig,
    ResponseHeaderRules, Route, RouteMatch, SharingMode, Upstream,
};
use crate::domain::plugin::plugin_registry_key;
use crate::domain::services::alias::{
    AliasResolution, common_domain_suffix, is_valid_hostname, normalize_alias, resolve_alias,
};
use crate::domain::services::management::{ControlPlaneStore, ManagementService};

/// The upstream an alias resolved to, with the configuration merged over the
/// tenant chain.
#[derive(Debug, Clone)]
pub struct MergedUpstream {
    /// The closest (shadowing) upstream definition.
    pub upstream: Upstream,
    /// Ancestor definitions of the same alias that are visible downward,
    /// nearest ancestor first.
    pub ancestors: Vec<Upstream>,
    /// Effective `headers` rules after tenant merging.
    pub headers: HeaderRules,
    /// Effective upstream-level plugin chain (root-most first).
    pub plugins: Vec<PluginBinding>,
    /// Effective upstream-level rate limit, or `None` when nothing limits.
    pub rate_limit: Option<RateLimitConfig>,
    /// Effective CORS configuration, or `None` when CORS is not configured.
    pub cors: Option<CorsConfig>,
}

impl MergedUpstream {
    /// The upstream that will be dialled.
    #[must_use]
    pub const fn upstream(&self) -> &Upstream {
        &self.upstream
    }

    /// Every endpoint of the resolved upstream.
    #[must_use]
    pub fn endpoints(&self) -> &[Endpoint] {
        self.upstream.endpoints()
    }
}

/// A route matched for a proxied request.
#[derive(Debug, Clone)]
pub struct MatchedRoute {
    /// The route resource.
    pub route: Route,
    /// The path to send upstream (suffix already applied).
    pub upstream_path: String,
    /// The query string to forward (already validated against the allow-list).
    pub upstream_query: String,
}

/// Outcome of route matching.
#[derive(Debug, Clone)]
pub enum RouteMatchResult {
    /// A route matched; proxy it.
    Matched(Box<MatchedRoute>),
    /// A path matched but its method list rejects the request (405).
    MethodNotAllowed {
        /// Comma-separated methods the matching routes serve.
        allow: String,
    },
}

/// Resolve an alias over the requesting tenant's chain.
///
/// The chain is `[self, parent, …, root]`; the store's alias lookup plus
/// [`resolve_alias`] implement ADR 0003 shadowing.
///
/// # Errors
/// * [`DomainError::RouteNotFound`] — no upstream in the chain owns the alias.
/// * [`DomainError::UpstreamDisabled`] — the closest definition is disabled.
pub fn resolve_upstream(
    service: &ManagementService,
    tenant_chain: &[Uuid],
    alias: &str,
) -> Result<MergedUpstream, DomainError> {
    let store: &Arc<dyn ControlPlaneStore> = service.store();
    let normalized = normalize_alias(alias);
    let candidates = store.alias_candidates(tenant_chain, &normalized);
    let resolved = match resolve_alias(&normalized, &candidates) {
        AliasResolution::Resolved(candidate) => candidate,
        AliasResolution::Disabled(disabled) => {
            let closest = disabled.first().cloned();
            let host = closest
                .as_ref()
                .and_then(|candidate| {
                    store
                        .get_upstream(candidate.tenant_id, candidate.upstream_id)
                        .ok()
                })
                .and_then(|upstream| upstream.hosts().into_iter().next());
            return Err(DomainError::UpstreamDisabled {
                upstream_id: closest.map_or_else(Uuid::nil, |candidate| candidate.upstream_id),
                host,
            });
        }
        AliasResolution::Unresolved => {
            return Err(DomainError::RouteNotFound {
                detail: format!(
                    "no upstream with alias `{normalized}` is reachable from this tenant"
                ),
            });
        }
    };
    let upstream = store.get_upstream(resolved.tenant_id, resolved.upstream_id)?;

    // Ancestors sit *after* the resolved entry in the descendant → root
    // ordering; each of them shadows nothing but may contribute configuration.
    let mut ancestors = Vec::new();
    for candidate in &candidates {
        if candidate.tenant_id == resolved.tenant_id {
            continue;
        }
        if let Ok(ancestor) = store.get_upstream(candidate.tenant_id, candidate.upstream_id) {
            ancestors.push(ancestor);
        }
    }

    Ok(merge_upstream(upstream, &ancestors))
}

/// Combine the shadowing upstream with its visible ancestors.
#[must_use]
pub fn merge_upstream(upstream: Upstream, ancestors: &[Upstream]) -> MergedUpstream {
    let header_blocks = ancestor_blocks(ancestors, |u| {
        u.headers.clone().map(|rules| (SharingMode::Inherit, rules))
    });
    let plugin_blocks = ancestor_blocks(ancestors, |u| {
        u.plugins.clone().map(|chain| (chain.sharing, chain))
    });
    let rate_blocks = ancestor_blocks(ancestors, |u| {
        u.rate_limit.clone().map(|limit| (limit.sharing, limit))
    });
    let cors_blocks = ancestor_blocks(ancestors, |u| {
        u.cors.clone().map(|cors| (cors.sharing, cors))
    });
    MergedUpstream {
        headers: merge_header_rules(
            &header_blocks,
            &upstream.headers.clone().unwrap_or_default(),
        ),
        plugins: merge_plugin_chains(
            &plugin_blocks,
            &upstream.plugins.clone().unwrap_or_default(),
        ),
        rate_limit: merge_rate_limits(&rate_blocks, upstream.rate_limit.as_ref()),
        cors: merge_optional_block(&cors_blocks, upstream.cors.clone()),
        upstream,
        ancestors: ancestors.to_vec(),
    }
}

/// Collect the `(sharing, block)` pairs of every ancestor that declares one.
///
/// Ancestors are ordered nearest first, so merge helpers that want "nearest
/// wins" iterate in order and merge helpers that want "root-most first"
/// iterate in reverse.
fn ancestor_blocks<T>(
    ancestors: &[Upstream],
    get: impl Fn(&Upstream) -> Option<(SharingMode, T)>,
) -> Vec<(SharingMode, T)> {
    ancestors
        .iter()
        .filter_map(|ancestor| {
            let (mode, block) = get(ancestor)?;
            if !is_visible_to_descendants(mode) {
                return None;
            }
            Some((mode, block))
        })
        .collect()
}

/// Merge header rules: ancestors first (root-most last), the child overrides.
///
/// Header rules carry no `sharing` of their own, so an ancestor's rules are
/// always overridable by the shadowing upstream.
#[must_use]
pub fn merge_header_rules(
    ancestors: &[(SharingMode, HeaderRules)],
    own: &HeaderRules,
) -> HeaderRules {
    let mut merged = HeaderRules::default();
    for (_mode, rules) in ancestors.iter().rev() {
        overlay_rules(&mut merged, rules);
    }
    overlay_rules(&mut merged, own);
    merged
}

/// Overlay `rules` onto `merged` (the later rules win per header name).
fn overlay_rules(merged: &mut HeaderRules, rules: &HeaderRules) {
    for (name, value) in &rules.request.set {
        merged.request.set.insert(name.clone(), value.clone());
    }
    for (name, value) in &rules.request.add {
        merged.request.add.insert(name.clone(), value.clone());
    }
    for name in &rules.request.remove {
        if !merged
            .request
            .remove
            .iter()
            .any(|existing| existing.eq_ignore_ascii_case(name))
        {
            merged.request.remove.push(name.clone());
        }
    }
    if rules.request.passthrough != crate::domain::model::HeaderPassthrough::None {
        merged.request.passthrough = rules.request.passthrough;
    }
    for name in &rules.request.passthrough_allowlist {
        if !merged
            .request
            .passthrough_allowlist
            .iter()
            .any(|existing| existing.eq_ignore_ascii_case(name))
        {
            merged.request.passthrough_allowlist.push(name.clone());
        }
    }
    for (name, value) in &rules.response.set {
        merged.response.set.insert(name.clone(), value.clone());
    }
    for (name, value) in &rules.response.add {
        merged.response.add.insert(name.clone(), value.clone());
    }
    for name in &rules.response.remove {
        if !merged
            .response
            .remove
            .iter()
            .any(|existing| existing.eq_ignore_ascii_case(name))
        {
            merged.response.remove.push(name.clone());
        }
    }
}

/// Merge response-header rules (used by the response phase).
#[must_use]
pub fn merge_response_rules(
    ancestors: &[(SharingMode, ResponseHeaderRules)],
    own: &ResponseHeaderRules,
) -> ResponseHeaderRules {
    let mut merged = ResponseHeaderRules::default();
    for (_mode, rules) in ancestors.iter().rev() {
        for (name, value) in &rules.set {
            merged.set.insert(name.clone(), value.clone());
        }
        for (name, value) in &rules.add {
            merged.add.insert(name.clone(), value.clone());
        }
        for name in &rules.remove {
            merged.remove.push(name.clone());
        }
    }
    for (name, value) in &own.set {
        merged.set.insert(name.clone(), value.clone());
    }
    for (name, value) in &own.add {
        merged.add.insert(name.clone(), value.clone());
    }
    for name in &own.remove {
        merged.remove.push(name.clone());
    }
    merged
}

/// Merge upstream-level plugin chains (root-most first, deduplicated by
/// plugin reference).
#[must_use]
pub fn merge_plugin_chains(
    ancestors: &[(SharingMode, PluginChain)],
    own: &PluginChain,
) -> Vec<PluginBinding> {
    // An ancestor's `enforce` chain replaces everything below it.
    if let Some((_mode, enforced)) = ancestors
        .iter()
        .find(|(mode, _)| *mode == SharingMode::Enforce)
    {
        let mut bindings: Vec<PluginBinding> = Vec::new();
        let mut seen: BTreeSet<String> = BTreeSet::new();
        push_chain(&mut bindings, &mut seen, enforced);
        return bindings;
    }
    let mut merged: Vec<PluginBinding> = Vec::new();
    let mut seen: BTreeSet<String> = BTreeSet::new();
    for (_mode, chain) in ancestors.iter().rev() {
        push_chain(&mut merged, &mut seen, chain);
    }
    push_chain(&mut merged, &mut seen, own);
    merged
}

fn push_chain(out: &mut Vec<PluginBinding>, seen: &mut BTreeSet<String>, chain: &PluginChain) {
    for binding in &chain.items {
        if seen.insert(binding.plugin_ref().to_owned()) {
            out.push(binding.clone());
        }
    }
}

/// Merge the route-level plugin chain onto the upstream-level one.
///
/// DESIGN "Plugin System": `[U1, U2] + [R1, R2] => [U1, U2, R1, R2]` — upstream
/// plugins execute before route plugins (ADR 0002 "Execution Order"). A route
/// binding for a reference the upstream already binds *overrides* it (the route
/// level wins on conflict) but keeps the upstream slot in the execution order.
///
/// Bindings without a usable reference are dropped, exactly as the upstream
/// merge does.
#[must_use]
pub fn merge_route_plugins(
    upstream: &[PluginBinding],
    route: Option<&PluginChain>,
) -> Vec<PluginBinding> {
    let mut merged: Vec<PluginBinding> = Vec::new();
    let mut seen: BTreeSet<String> = BTreeSet::new();
    for binding in upstream {
        let key = plugin_registry_key(binding.plugin_ref());
        if key.is_empty() || !seen.insert(key) {
            continue;
        }
        merged.push(binding.clone());
    }
    let Some(route) = route else {
        return merged;
    };
    for binding in &route.items {
        let key = plugin_registry_key(binding.plugin_ref());
        if key.is_empty() {
            continue;
        }
        if seen.insert(key.clone()) {
            merged.push(binding.clone());
        } else if let Some(slot) = merged
            .iter_mut()
            .find(|existing| plugin_registry_key(existing.plugin_ref()) == key)
        {
            // The route level wins on conflict.
            *slot = binding.clone();
        }
    }
    merged
}

/// Merge rate limits as `min(parent, child)` (ADR 0003), honouring `enforce`.
#[must_use]
pub fn merge_rate_limits(
    ancestors: &[(SharingMode, RateLimitConfig)],
    own: Option<&RateLimitConfig>,
) -> Option<RateLimitConfig> {
    // An ancestor's `enforce` block cannot be overridden by a descendant.
    if let Some((_mode, enforced)) = ancestors
        .iter()
        .find(|(mode, _)| *mode == SharingMode::Enforce)
    {
        return Some(enforced.clone());
    }
    let mut effective = own.cloned();
    // Ancestors are ordered nearest first; the nearest wins ties.
    for (_mode, block) in ancestors {
        effective = Some(match effective {
            None => block.clone(),
            Some(current) => tighter(current, block.clone()),
        });
    }
    effective
}

/// The more restrictive of two rate-limit configurations.
fn tighter(current: RateLimitConfig, other: RateLimitConfig) -> RateLimitConfig {
    let burst = current.effective_burst().min(other.effective_burst());
    if per_second(&other) < per_second(&current) {
        RateLimitConfig {
            burst: Some(crate::domain::model::BurstCapacity { capacity: burst }),
            ..other
        }
    } else {
        RateLimitConfig {
            burst: Some(crate::domain::model::BurstCapacity { capacity: burst }),
            ..current
        }
    }
}

/// Sustained rate normalised to requests per second.
fn per_second(config: &RateLimitConfig) -> f64 {
    let window = config.sustained.window.as_secs().max(1) as f64;
    config.sustained.rate.max(1) as f64 / window
}

/// Merge a generic optional block: an `enforce` ancestor wins, then the
/// shadowing upstream's own definition, then the nearest ancestor that
/// declares one.
#[must_use]
pub fn merge_optional_block<T: Clone>(ancestors: &[(SharingMode, T)], own: Option<T>) -> Option<T> {
    if let Some((_mode, enforced)) = ancestors
        .iter()
        .find(|(mode, _)| *mode == SharingMode::Enforce)
    {
        return Some(enforced.clone());
    }
    if own.is_some() {
        return own;
    }
    ancestors.first().map(|(_mode, block)| block.clone())
}

/// Pick the endpoint to dial (ADR 0001 target-host matrix).
///
/// `target_host` is the client's `X-OAGW-Target-Host` header, if any;
/// `round_robin` is the per-upstream counter used when the header is absent
/// and the alias is not a common-suffix alias.
///
/// # Errors
/// * [`DomainError::MissingTargetHost`] — a multi-endpoint common-suffix alias
///   needs the header to disambiguate.
/// * [`DomainError::InvalidTargetHost`] — the header is present but is not a
///   hostname or IP address.
/// * [`DomainError::UnknownTargetHost`] — the header names no configured
///   endpoint.
pub fn select_endpoint(
    upstream: &Upstream,
    target_host: Option<&str>,
    round_robin: usize,
) -> Result<Endpoint, DomainError> {
    let endpoints = upstream.endpoints();
    if endpoints.is_empty() {
        return Err(DomainError::MissingTargetHost {
            upstream_id: upstream.id,
            alias: upstream.alias.clone(),
        });
    }
    // ADR 0007: both `routing.invalid_target_host.v1` and
    // `routing.unknown_target_host.v1` carry the hosts the header could have
    // named, so the client can correct it without a second round trip.
    let valid_hosts = upstream.distinct_hosts();
    if let Some(raw) = target_host {
        let host = normalize_host(raw);
        if !is_valid_hostname(&host) {
            return Err(invalid_target_host(
                upstream,
                raw,
                "X-OAGW-Target-Host must be a valid hostname or IP address (no port, path, or special characters)",
                valid_hosts,
            ));
        }
        return endpoints
            .iter()
            .find(|endpoint| normalize_host(&endpoint.host) == host)
            .cloned()
            .ok_or_else(|| {
                tracing::debug!(upstream = %upstream.id, value = raw, "target host not configured");
                DomainError::UnknownTargetHost {
                    upstream_id: upstream.id,
                    invalid_value: raw.to_owned(),
                    valid_hosts,
                }
            });
    }
    // A common-suffix alias (e.g. `vendor.com` over `us.vendor.com` /
    // `eu.vendor.com`) is ambiguous without the header.
    if upstream_alias_is_common_suffix(upstream) {
        return Err(DomainError::MissingTargetHost {
            upstream_id: upstream.id,
            alias: upstream.alias.clone(),
        });
    }
    let index = if endpoints.len() == 1 {
        0
    } else {
        round_robin % endpoints.len()
    };
    Ok(endpoints[index].clone())
}

/// Build the 400 [`DomainError::InvalidTargetHost`] for a rejected
/// `X-OAGW-Target-Host` header. `reason` is logged, never returned to the
/// client as-is (the problem document carries the valid hosts instead).
fn invalid_target_host(
    upstream: &Upstream,
    value: &str,
    reason: &str,
    valid_hosts: Vec<String>,
) -> DomainError {
    tracing::debug!(upstream = %upstream.id, value = %value, reason, "target host rejected");
    DomainError::InvalidTargetHost {
        upstream_id: upstream.id,
        invalid_value: value.to_owned(),
        valid_hosts,
    }
}

/// True when `upstream.alias` is a shared registrable-domain suffix of all its
/// endpoint hosts (ADR 0001 "common suffix alias").
#[must_use]
pub fn upstream_alias_is_common_suffix(upstream: &Upstream) -> bool {
    let hosts = upstream.distinct_hosts();
    if hosts.len() < 2 {
        return false;
    }
    let alias = normalize_alias(&upstream.alias);
    if alias.is_empty() {
        return false;
    }
    // A non-standard port makes a derived alias `host:port`; the ambiguity is
    // about the *host*, so the port suffix never takes part in the comparison.
    let alias_host = match alias.rsplit_once(':') {
        Some((host, port)) if !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()) => host,
        _ => alias.as_str(),
    };
    if let Some(suffix) = common_domain_suffix(&hosts) {
        return alias_host.eq_ignore_ascii_case(&suffix);
    }
    // Not every shared suffix is a registrable domain per the PSL; accept a
    // literal shared suffix too, so `us.vendor.internal` / `eu.vendor.internal`
    // still requires disambiguation.
    hosts.iter().all(|host| {
        host == alias_host
            || host
                .strip_suffix(alias_host)
                .is_some_and(|prefix| prefix.ends_with('.'))
    })
}

/// Tenant chain for a request: the requesting tenant first, then its
/// ancestors ordered direct parent → root.
#[must_use]
pub fn chain_from(tenant_id: Uuid, ancestors: &[Uuid]) -> Vec<Uuid> {
    let mut chain = vec![tenant_id];
    for ancestor in ancestors {
        if *ancestor != tenant_id && !chain.contains(ancestor) {
            chain.push(*ancestor);
        }
    }
    chain
}

/// True when a resource with `mode` is visible to descendants.
#[must_use]
pub const fn is_visible_to_descendants(mode: SharingMode) -> bool {
    !matches!(mode, SharingMode::Private)
}

/// Match a request against a tenant's routes for `upstream_id`.
///
/// The route `match.http.path` is a segment-aware prefix of the request path;
/// the longest match wins. A path-matching route whose method list rejects the
/// request yields [`RouteMatchResult::MethodNotAllowed`] with the `Allow` list.
///
/// # Errors
/// * [`DomainError::RouteNotFound`] — no route path matches.
/// * [`DomainError::Validation`] — query allow-list or path-suffix guard hit.
pub fn match_route(
    store: &dyn ControlPlaneStore,
    tenant_id: Uuid,
    upstream: &Upstream,
    method: &str,
    request_path: &str,
    query: &str,
) -> Result<RouteMatchResult, DomainError> {
    let request_path = normalize_request_path(request_path);
    // An HTTP HEAD is a GET without a response body.
    let effective_method = if method.eq_ignore_ascii_case("HEAD") {
        "GET"
    } else {
        method
    };

    let mut best: Option<(usize, Route)> = None;
    let mut allowed_methods: BTreeSet<String> = BTreeSet::new();

    for route in store.list_routes(tenant_id) {
        if !route.enabled || route.upstream_id != upstream.id {
            continue;
        }
        let RouteMatch::Http(http) = &route.match_config else {
            continue;
        };
        let Some(_) = path_prefix_remainder(&http.path, &request_path) else {
            continue;
        };
        if !http.methods.is_empty()
            && !http
                .methods
                .iter()
                .any(|candidate| candidate.as_str().eq_ignore_ascii_case(effective_method))
        {
            for served in &http.methods {
                allowed_methods.insert(served.as_str().to_owned());
            }
            continue;
        }
        let score = segment_count(&http.path);
        if best
            .as_ref()
            .is_none_or(|(best_score, _)| score >= *best_score)
        {
            best = Some((score, route));
        }
    }

    let Some((_score, route)) = best else {
        if allowed_methods.is_empty() {
            return Err(DomainError::RouteNotFound {
                detail: format!(
                    "no route of upstream `{}` matches {method} {request_path}",
                    upstream.alias
                ),
            });
        }
        let mut list: Vec<String> = allowed_methods.iter().cloned().collect();
        list.sort();
        return Ok(RouteMatchResult::MethodNotAllowed {
            allow: list.join(", "),
        });
    };

    let RouteMatch::Http(http) = route.match_config.clone() else {
        return Err(DomainError::RouteNotFound {
            detail: "the matched route is not an HTTP route".to_owned(),
        });
    };

    let remainder = path_prefix_remainder(&http.path, &request_path).unwrap_or_default();
    let upstream_path =
        if http.path_suffix_mode == crate::domain::model::route::PathSuffixMode::Disabled {
            if !remainder.is_empty() {
                return Err(DomainError::validation(
                    "path",
                    "path_suffix_mode is `disabled` for this route; the path suffix must be empty",
                ));
            }
            http.path.clone()
        } else {
            format!("{}{}", http.path, remainder)
        };

    // Query guard (DESIGN "Guard Rules"): an empty allow-list allows none.
    if !query.is_empty()
        && let Some(offending) = form_urlencoded::parse(query.as_bytes())
            .map(|(name, _value)| name.into_owned())
            .find(|name| {
                !http
                    .query_allowlist
                    .iter()
                    .any(|allowed| allowed.eq_ignore_ascii_case(name))
            })
    {
        return Err(DomainError::validation(
            "query",
            format!(
                "query parameter `{offending}` is not allowed by this route (allowed: {:?})",
                http.query_allowlist
            ),
        ));
    }

    Ok(RouteMatchResult::Matched(Box::new(MatchedRoute {
        route,
        upstream_path,
        upstream_query: query.to_owned(),
    })))
}

/// Normalise a request path: always starts with `/`, never ends with `/`
/// unless it is exactly `/`.
#[must_use]
pub fn normalize_request_path(path: &str) -> String {
    let trimmed = path.trim();
    if trimmed.is_empty() {
        return "/".to_owned();
    }
    let with_slash = if trimmed.starts_with('/') {
        trimmed.to_owned()
    } else {
        format!("/{trimmed}")
    };
    if with_slash == "/" {
        return with_slash;
    }
    with_slash.trim_end_matches('/').to_owned()
}

/// The remainder of `request_path` after the segment-aware prefix `pattern`,
/// or `None` when `pattern` is not a prefix of it.
///
/// The remainder starts with `/` (so `pattern + remainder` is an absolute
/// path) and is empty when the request path *is* the pattern.
#[must_use]
pub fn path_prefix_remainder(pattern: &str, request_path: &str) -> Option<String> {
    let pattern = normalize_request_path(pattern);
    if pattern == "/" {
        // The root pattern matches everything.
        return Some(request_path[1..].to_owned());
    }
    if request_path == pattern {
        return Some(String::new());
    }
    let prefix = format!("{pattern}/");
    request_path
        .strip_prefix(&prefix)
        .map(|tail| format!("/{tail}"))
}

/// Number of `/`-separated segments in a path pattern ("longest match wins").
#[must_use]
pub fn segment_count(path: &str) -> usize {
    path.split('/')
        .filter(|segment| !segment.is_empty())
        .count()
}

/// The `auth` binding of an upstream, for diagnostics.
#[must_use]
pub fn auth_binding(upstream: &Upstream) -> Option<&AuthBinding> {
    upstream.auth.as_ref()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::model::route::{HttpMatch, HttpMethod};
    use crate::domain::model::{Scheme, ServerConfig};
    use crate::infra::storage::MemoryStorage;

    fn upstream(alias: &str, hosts: &[&str]) -> Upstream {
        Upstream {
            alias: alias.to_owned(),
            server: ServerConfig {
                endpoints: hosts
                    .iter()
                    .map(|host| Endpoint {
                        scheme: Scheme::Https,
                        host: (*host).to_owned(),
                        port: None,
                    })
                    .collect(),
            },
            ..Upstream::default()
        }
    }

    #[test]
    fn chain_is_deduplicated() {
        let tenant = Uuid::new_v4();
        let parent = Uuid::new_v4();
        assert_eq!(
            chain_from(tenant, &[parent, parent, tenant]),
            vec![tenant, parent]
        );
    }

    #[test]
    fn visibility() {
        assert!(is_visible_to_descendants(SharingMode::Inherit));
        assert!(is_visible_to_descendants(SharingMode::Enforce));
        assert!(!is_visible_to_descendants(SharingMode::Private));
    }

    #[test]
    fn single_endpoint_needs_no_header() {
        let target = upstream("api.openai.com", &["api.openai.com"]);
        let endpoint = select_endpoint(&target, None, 0).unwrap();
        assert_eq!(endpoint.host, "api.openai.com");
    }

    #[test]
    fn multi_endpoint_explicit_alias_round_robins() {
        let target = upstream(
            "my-service",
            &["server-a.example.com", "server-b.example.com"],
        );
        assert_eq!(
            select_endpoint(&target, None, 0).unwrap().host,
            "server-a.example.com"
        );
        assert_eq!(
            select_endpoint(&target, None, 1).unwrap().host,
            "server-b.example.com"
        );
        assert_eq!(
            select_endpoint(&target, None, 2).unwrap().host,
            "server-a.example.com"
        );
        assert_eq!(
            select_endpoint(&target, Some("server-b.example.com"), 0)
                .unwrap()
                .host,
            "server-b.example.com"
        );
    }

    #[test]
    fn common_suffix_alias_requires_the_header() {
        let target = upstream("vendor.com", &["us.vendor.com", "eu.vendor.com"]);
        let err = select_endpoint(&target, None, 0).unwrap_err();
        assert_eq!(err.status(), 400);
        assert!(err.problem_type().ends_with("missing_target_host.v1"));
        let err = select_endpoint(&target, Some("apac.vendor.com"), 0).unwrap_err();
        assert_eq!(err.status(), 400);
        let err = select_endpoint(&target, Some("us.vendor.com:443"), 0).unwrap_err();
        assert_eq!(err.status(), 400);
    }

    #[test]
    fn path_prefixes_are_segment_aware() {
        assert_eq!(
            path_prefix_remainder("/v1", "/v1/chat"),
            Some("/chat".to_owned())
        );
        assert_eq!(path_prefix_remainder("/v1", "/v11"), None);
        assert_eq!(
            path_prefix_remainder("/", "/anything"),
            Some("anything".to_owned())
        );
        assert_eq!(path_prefix_remainder("/v1", "/v1"), Some(String::new()));
    }

    #[test]
    fn request_paths_are_normalised() {
        assert_eq!(normalize_request_path(""), "/".to_owned());
        assert_eq!(normalize_request_path("/"), "/".to_owned());
        assert_eq!(normalize_request_path("a/b"), "/a/b".to_owned());
        assert_eq!(normalize_request_path("/a/b/"), "/a/b".to_owned());
        assert_eq!(segment_count("/v1/chat/completions"), 3);
    }

    #[test]
    fn rate_limits_take_the_minimum() {
        let parent = RateLimitConfig {
            sustained: crate::domain::model::SustainedRate {
                rate: 10,
                window: crate::domain::model::RateWindow::Second,
            },
            ..RateLimitConfig::default()
        };
        let child = RateLimitConfig {
            sustained: crate::domain::model::SustainedRate {
                rate: 100,
                window: crate::domain::model::RateWindow::Minute,
            },
            ..RateLimitConfig::default()
        };
        let merged = merge_rate_limits(&[(SharingMode::Inherit, parent)], Some(&child)).unwrap();
        // 100/minute is *tighter* than 10/second, so the child block wins.
        assert_eq!(
            (merged.sustained.rate, merged.sustained.window),
            (100, crate::domain::model::RateWindow::Minute)
        );
        assert_eq!(merged.effective_burst(), 10);

        // With the same window the numeric minimum wins.
        let parent = RateLimitConfig {
            sustained: crate::domain::model::SustainedRate {
                rate: 10,
                window: crate::domain::model::RateWindow::Second,
            },
            ..RateLimitConfig::default()
        };
        let child = RateLimitConfig {
            sustained: crate::domain::model::SustainedRate {
                rate: 100,
                window: crate::domain::model::RateWindow::Second,
            },
            ..RateLimitConfig::default()
        };
        let merged = merge_rate_limits(&[(SharingMode::Inherit, parent)], Some(&child)).unwrap();
        assert_eq!(merged.sustained.rate, 10);
    }

    #[test]
    fn enforced_ancestor_rate_limit_wins() {
        let enforced = RateLimitConfig {
            sustained: crate::domain::model::SustainedRate {
                rate: 1,
                window: crate::domain::model::RateWindow::Minute,
            },
            ..RateLimitConfig::default()
        };
        let child = RateLimitConfig::default();
        let merged = merge_rate_limits(&[(SharingMode::Enforce, enforced.clone())], Some(&child));
        assert_eq!(merged, Some(enforced));
    }

    #[test]
    fn header_rules_overlay() {
        let mut ancestor = HeaderRules::default();
        ancestor
            .request
            .set
            .insert("x-a".to_owned(), "1".to_owned());
        let mut own = HeaderRules::default();
        own.request.set.insert("x-a".to_owned(), "2".to_owned());
        own.request.set.insert("x-b".to_owned(), "3".to_owned());
        let merged = merge_header_rules(&[(SharingMode::Inherit, ancestor)], &own);
        assert_eq!(merged.request.set.get("x-a").map(String::as_str), Some("2"));
        assert_eq!(merged.request.set.get("x-b").map(String::as_str), Some("3"));
    }

    #[test]
    fn plugin_chains_are_deduplicated() {
        let mut ancestor = PluginChain::default();
        ancestor
            .items
            .push(crate::domain::model::PluginBinding::Bare("p".to_owned()));
        let mut own = PluginChain::default();
        own.items
            .push(crate::domain::model::PluginBinding::Bare("p".to_owned()));
        own.items
            .push(crate::domain::model::PluginBinding::Bare("q".to_owned()));
        let merged = merge_plugin_chains(&[(SharingMode::Inherit, ancestor)], &own);
        assert_eq!(merged.len(), 2);
        assert_eq!(merged[0].plugin_ref(), "p");
        assert_eq!(merged[1].plugin_ref(), "q");
    }

    #[test]
    fn route_match_is_typed() {
        let route = Route::default();
        assert!(route.match_config.is_http());
        assert!(matches!(route.match_config, RouteMatch::Http(_)));
    }

    /// A store seeded with routes, for `match_route`.
    fn route_store(
        tenant: Uuid,
        upstream: &Upstream,
        paths: &[(&str, HttpMethod)],
    ) -> MemoryStorage {
        let store = MemoryStorage::new();
        for (path, method) in paths {
            let mut route = Route {
                match_config: RouteMatch::Http(HttpMatch {
                    methods: vec![*method],
                    path: (*path).to_owned(),
                    ..HttpMatch::default()
                }),
                ..Route::default()
            };
            route.tenant_id = tenant;
            route.upstream_id = upstream.id;
            store.insert_route(tenant, route).expect("seed route");
        }
        store
    }

    #[test]
    fn an_exact_path_beats_a_shorter_prefix() {
        let tenant = Uuid::new_v4();
        let target = upstream("api.vendor.com", &["api.vendor.com"]);
        let store = route_store(
            tenant,
            &target,
            &[
                ("/v1", HttpMethod::Post),
                ("/v1/chat/completions", HttpMethod::Post),
            ],
        );

        let matched = match match_route(&store, tenant, &target, "POST", "/v1/chat/completions", "")
            .expect("a route matches")
        {
            RouteMatchResult::Matched(matched) => matched,
            RouteMatchResult::MethodNotAllowed { .. } => panic!("the method is served"),
        };
        // The segment-aware prefix with the most segments wins, so the exact
        // route beats the `/v1` prefix and no suffix is left to append.
        assert_eq!(matched.route.match_key(), "http POST /v1/chat/completions");
        assert_eq!(matched.upstream_path, "/v1/chat/completions");

        // A less specific request still falls back to the prefix route.
        let matched = match match_route(&store, tenant, &target, "POST", "/v1/embeddings", "")
            .expect("a route matches")
        {
            RouteMatchResult::Matched(matched) => matched,
            RouteMatchResult::MethodNotAllowed { .. } => panic!("the method is served"),
        };
        assert_eq!(matched.upstream_path, "/v1/embeddings");
    }

    #[test]
    fn a_path_no_route_prefixes_is_route_not_found() {
        let tenant = Uuid::new_v4();
        let target = upstream("api.vendor.com", &["api.vendor.com"]);
        let store = route_store(tenant, &target, &[("/v1", HttpMethod::Post)]);

        let err = match_route(&store, tenant, &target, "POST", "/other", "").unwrap_err();
        assert_eq!(err.status(), 404);
        assert!(err.problem_type().ends_with("route.not_found.v1"));

        // A route of another upstream never matches this one.
        let other = upstream("other.vendor.com", &["other.vendor.com"]);
        assert!(match_route(&store, tenant, &other, "POST", "/v1", "").is_err());
    }

    #[test]
    fn a_disabled_route_is_never_matched() {
        let tenant = Uuid::new_v4();
        let target = upstream("api.vendor.com", &["api.vendor.com"]);
        let store = MemoryStorage::new();
        let mut route = Route {
            match_config: RouteMatch::Http(HttpMatch {
                methods: vec![HttpMethod::Get],
                path: "/v1".to_owned(),
                ..HttpMatch::default()
            }),
            ..Route::default()
        };
        route.tenant_id = tenant;
        route.upstream_id = target.id;
        route.enabled = false;
        store.insert_route(tenant, route).expect("seed route");
        assert!(match_route(&store, tenant, &target, "GET", "/v1", "").is_err());
    }

    #[test]
    fn an_enforced_ancestor_plugin_chain_replaces_everything_below_it() {
        let mut enforced = PluginChain::default();
        enforced
            .items
            .push(PluginBinding::Bare("enforced".to_owned()));
        let mut own = PluginChain::default();
        own.items.push(PluginBinding::Bare("own".to_owned()));
        let merged = merge_plugin_chains(&[(SharingMode::Enforce, enforced)], &own);
        let refs: Vec<&str> = merged.iter().map(PluginBinding::plugin_ref).collect();
        assert_eq!(refs, ["enforced"]);
    }

    #[test]
    fn a_private_ancestor_contributes_nothing() {
        let mut ancestor = PluginChain {
            sharing: SharingMode::Private,
            ..PluginChain::default()
        };
        ancestor
            .items
            .push(PluginBinding::Bare("ancestor".to_owned()));
        let mut own = PluginChain::default();
        own.items.push(PluginBinding::Bare("own".to_owned()));

        let mut shadowing = upstream("api.vendor.com", &["api.vendor.com"]);
        shadowing.plugins = Some(own);
        let mut inherited = upstream("api.vendor.com", &["api.vendor.com"]);
        inherited.plugins = Some(ancestor);

        // A `private` ancestor is invisible to descendants, so only the own
        // chain runs (PRD "configuration layering").
        let merged = merge_upstream(shadowing, &[inherited]);
        let refs: Vec<&str> = merged
            .plugins
            .iter()
            .map(PluginBinding::plugin_ref)
            .collect();
        assert_eq!(refs, ["own"]);
    }

    #[test]
    fn an_ancestor_chain_runs_root_most_first() {
        let mut root = PluginChain::default();
        root.items.push(PluginBinding::Bare("root".to_owned()));
        let mut parent = PluginChain::default();
        parent.items.push(PluginBinding::Bare("parent".to_owned()));
        let mut own = PluginChain::default();
        own.items.push(PluginBinding::Bare("own".to_owned()));
        // Ancestors are ordered nearest first; the merged chain is root-most
        // first so the outermost plugin wraps the innermost one.
        let merged = merge_plugin_chains(
            &[(SharingMode::Inherit, parent), (SharingMode::Inherit, root)],
            &own,
        );
        let refs: Vec<&str> = merged.iter().map(PluginBinding::plugin_ref).collect();
        assert_eq!(refs, ["root", "parent", "own"]);
    }

    fn binding(reference: &str) -> PluginBinding {
        PluginBinding::Bare(reference.to_owned())
    }

    fn config_binding(reference: &str, key: &str, value: &str) -> PluginBinding {
        PluginBinding::Detailed {
            plugin_ref: reference.to_owned(),
            config: Some(serde_json::json!({key: value})),
        }
    }

    fn chain(items: Vec<PluginBinding>) -> PluginChain {
        PluginChain {
            sharing: SharingMode::Private,
            items,
        }
    }

    #[test]
    fn route_plugins_are_appended_after_the_upstream_ones() {
        // DESIGN "Plugin System": `[U1, U2] + [R1, R2] => [U1, U2, R1, R2]`.
        let upstream = [binding("u1"), binding("u2")];
        let route = chain(vec![binding("r1"), binding("r2")]);
        let merged = merge_route_plugins(&upstream, Some(&route));
        let refs: Vec<&str> = merged.iter().map(PluginBinding::plugin_ref).collect();
        assert_eq!(refs, ["u1", "u2", "r1", "r2"]);
    }

    #[test]
    fn a_route_binding_overrides_the_same_upstream_reference() {
        let upstream = [
            config_binding("gts.cf.core.oagw.guard_plugin.v1~required", "want", "x-a"),
            binding("other"),
        ];
        let route = chain(vec![config_binding(
            "gts.cf.core.oagw.guard_plugin.v1~required",
            "want",
            "x-b",
        )]);
        let merged = merge_route_plugins(&upstream, Some(&route));
        let refs: Vec<&str> = merged.iter().map(PluginBinding::plugin_ref).collect();
        assert_eq!(
            refs,
            ["gts.cf.core.oagw.guard_plugin.v1~required", "other"],
            "the overridden plugin runs once, in the upstream slot"
        );
        // The route level wins on conflict, so the effective config is the
        // route's.
        assert_eq!(
            merged[0].config().and_then(|config| config.get("want")),
            Some(&serde_json::json!("x-b"))
        );
    }

    #[test]
    fn a_route_without_plugins_leaves_the_upstream_chain_untouched() {
        let upstream = [binding("u1"), binding("u1"), binding("u2")];
        let merged = merge_route_plugins(&upstream, None);
        let refs: Vec<&str> = merged.iter().map(PluginBinding::plugin_ref).collect();
        assert_eq!(refs, ["u1", "u2"], "the upstream chain stays deduplicated");
    }

    #[test]
    fn a_derived_alias_with_a_port_is_ambiguous_for_a_multi_host_upstream() {
        // `vendor.internal` is not a registrable domain per the PSL, so the
        // literal shared suffix still makes the alias ambiguous — and the
        // derived alias carries a port.
        let target = upstream(
            "vendor.internal:8443",
            &["us.vendor.internal", "eu.vendor.internal"],
        );
        assert!(upstream_alias_is_common_suffix(&target));
        let err = select_endpoint(&target, None, 0).unwrap_err();
        assert_eq!(err.status(), 400);
        assert!(err.problem_type().ends_with("missing_target_host.v1"));

        // A single-host upstream is never ambiguous, whatever its alias.
        let single = upstream("us.vendor.internal:8443", &["us.vendor.internal"]);
        assert!(!upstream_alias_is_common_suffix(&single));
        assert_eq!(
            select_endpoint(&single, None, 0).unwrap().host,
            "us.vendor.internal"
        );
    }
}
