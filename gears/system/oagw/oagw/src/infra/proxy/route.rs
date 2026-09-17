//! Alias resolution, route matching, and the configuration layering of
//! `docs/PRD.md` §5.5.
//!
//! ## Alias resolution
//!
//! `resolve_alias` walks the tenant chain (descendant → root,
//! `docs/DESIGN.md` — "Proxy (data plane) … Inherited via tenant chain walk")
//! and returns the **closest enabled upstream** that owns the alias, so a
//! descendant's own upstream shadows an ancestor's.
//!
//! ## Route matching
//!
//! Route matching follows `docs/ADR/0001`:
//!
//! * the upstream's `protocol` selects the match strategy (`Http` today; the
//!   `Grpc` catalogue entry has no reachable proxy path);
//! * HTTP matching is a method allowlist plus a **longest path prefix** match;
//! * ties are broken by ascending `priority`, then by ascending `created_at`;
//! * `path_suffix_mode: disabled` matches the pattern exactly and forwards the
//!   pattern upstream; `append` matches the pattern as a prefix and forwards
//!   the full inbound path upstream;
//! * a non-empty `query_allowlist` requires every request query parameter to
//!   be listed;
//! * disabled routes never match.
use crate::domain::model::{
    BurstConfig, CorsConfig, HttpMatch, PathSuffixMode, Protocol, RateLimitConfig, Route,
    RouteMatcher, SustainedRate, Upstream,
};
use crate::infra::plugin::{AuthBinding, PluginBinding};
use crate::infra::proxy::failure::ProxyFailure;

/// An alias resolved against a tenant chain.
#[derive(Debug, Clone)]
pub struct ResolvedUpstream {
    /// The matching upstream.
    pub upstream: Upstream,
    /// How far up the chain it was found: `0` is the caller's own tenant.
    pub depth: usize,
}

/// The endpoint a hop dials, with the selection's provenance.
#[derive(Debug, Clone)]
pub struct SelectedEndpoint {
    /// The endpoint the gateway dials.
    pub endpoint: crate::domain::model::Endpoint,
    /// Position in the endpoint pool.
    pub position: usize,
    /// `true` when the choice was made by the round-robin cursor rather than
    /// pinned by `X-OAGW-Target-Host` or by a single-endpoint pool.
    pub balanced: bool,
}

/// Configuration the data plane executes, after layering.
#[derive(Debug, Clone, Default)]
pub struct EffectiveConfig {
    /// Plugin chain: upstream bindings first, then route bindings.
    pub plugins: Vec<PluginBinding>,
    /// Guard bindings only.
    pub guards: Vec<PluginBinding>,
    /// Transform bindings only.
    pub transforms: Vec<PluginBinding>,
    /// The single auth binding, when the upstream declares one.
    pub auth: Option<AuthBinding>,
    /// Effective rate limit, or `None` when neither layer configures one.
    pub rate_limit: Option<RateLimitConfig>,
    /// Effective CORS policy, or `None` when neither layer configures one.
    pub cors: Option<CorsConfig>,
    /// Request/response header rewriting rules.
    pub headers: Option<crate::domain::model::HeadersConfig>,
}

/// Pick the closest upstream carrying `alias`, walking the chain from the
/// caller's tenant towards the root.
///
/// `chain` is ordered caller-first; the first hit wins, whether it is enabled
/// or not — a disabled upstream is *resolved* and then refused with a 503, so a
/// descendant's switched-off upstream never silently falls through to an
/// ancestor's configuration.
#[must_use]
pub fn resolve_alias(alias: &str, chain: &[Upstream]) -> Option<ResolvedUpstream> {
    chain
        .iter()
        .enumerate()
        .find(|(_, upstream)| upstream.alias == alias)
        .map(|(depth, upstream)| ResolvedUpstream {
            upstream: upstream.clone(),
            depth,
        })
}

/// Longest-prefix score of a pattern against a path, or `None` when the
/// pattern does not match.
///
/// A pattern is a path prefix: `/v1/*`, `/v1` and `/` all match `/v1/chat`
/// (ADR 0001 routes by "method allowlist + longest path prefix", and the
/// `append` suffix mode is what grants the suffix — the trailing `*` is an
/// accepted, optional spelling of the same thing). The score is the matched
/// prefix length, which is what makes the longest prefix win.
#[must_use]
pub fn prefix_match(pattern: &str, path: &str) -> Option<usize> {
    let prefix = pattern.trim_end_matches('*').trim_end_matches('/');
    if path == prefix || (prefix.is_empty() && path.starts_with('/')) {
        return Some(prefix.len());
    }
    path.strip_prefix(prefix)
        .filter(|suffix| suffix.starts_with('/') || suffix.is_empty())
        .map(|_| prefix.len())
}

/// Whether the request path is exactly the pattern.
#[must_use]
pub fn exact_match(pattern: &str, path: &str) -> bool {
    pattern == path
}

/// The upstream path a matched route forwards to.
#[must_use]
pub fn upstream_path(matcher: &HttpMatch, path: &str) -> String {
    match matcher.path_suffix_mode {
        PathSuffixMode::Disabled => matcher.path.to_owned(),
        PathSuffixMode::Append => {
            let prefix = matcher.path.trim_end_matches('*').trim_end_matches('/');
            if path.len() > prefix.len() {
                path.to_owned()
            } else {
                prefix.to_owned()
            }
        }
    }
}

/// Whether the request query is covered by the matcher's allowlist.
#[must_use]
pub fn query_allowed(matcher: &HttpMatch, query: Option<&str>) -> bool {
    if matcher.query_allowlist.is_empty() {
        return true;
    }
    let Some(query) = query.filter(|query| !query.is_empty()) else {
        return true;
    };
    form_urlencoded::parse(query.as_bytes())
        .map(|(name, _)| name.to_ascii_lowercase().to_string())
        .all(|name| {
            matcher
                .query_allowlist
                .iter()
                .any(|allowed| allowed.to_ascii_lowercase() == name)
        })
}

/// The best matching route of an upstream, or `None`.
#[must_use]
pub fn match_route<'r>(
    upstream: &Upstream,
    routes: &'r [Route],
    method: &str,
    path: &str,
    query: Option<&str>,
) -> Option<&'r Route> {
    routes
        .iter()
        .filter(|route| route.upstream_id == upstream.id && route.enabled)
        .filter(|route| matches_matcher(&route.matcher, upstream, method, path, query))
        .max_by(|left, right| {
            specificity(left)
                .cmp(&specificity(right))
                .then_with(|| right.priority.cmp(&left.priority))
                .then_with(|| right.created_at.cmp(&left.created_at))
        })
}

fn matches_matcher(
    matcher: &RouteMatcher,
    upstream: &Upstream,
    method: &str,
    path: &str,
    query: Option<&str>,
) -> bool {
    if !matches!(upstream.protocol, Protocol::Http) {
        return false;
    }
    let RouteMatcher::Http(http_matcher) = matcher else {
        return false;
    };
    method_allowed(http_matcher, method)
        && match_shape(http_matcher, path)
        && query_allowed(http_matcher, query)
}

fn method_allowed(matcher: &HttpMatch, method: &str) -> bool {
    matcher
        .methods
        .iter()
        .any(|allowed| allowed.eq_ignore_ascii_case(method))
}

fn match_shape(matcher: &HttpMatch, path: &str) -> bool {
    match matcher.path_suffix_mode {
        PathSuffixMode::Disabled => exact_match(&matcher.path, path),
        PathSuffixMode::Append => prefix_match(&matcher.path, path).is_some(),
    }
}

/// Route specificity used to break ties: the longest pattern wins.
fn specificity(route: &Route) -> usize {
    match &route.matcher {
        RouteMatcher::Http(http_matcher) => http_matcher.path.trim_end_matches('*').len(),
        RouteMatcher::Grpc(grpc_matcher) => grpc_matcher.service.len(),
    }
}

/// Layer the upstream's and the route's configuration.
///
/// Plugin chains concatenate (upstream first, then route, so upstream plugins
/// run before route plugins); rate limits and CORS are layered per
/// `docs/ADR/0003` and `docs/ADR/0004`; header rewriting comes from the
/// upstream.
#[must_use]
pub fn effective_config(upstream: &Upstream, route: Option<&Route>) -> EffectiveConfig {
    let mut plugins = engine_bindings(
        upstream
            .plugins
            .as_ref()
            .map(|plugins| plugins.items.as_slice())
            .unwrap_or_default(),
    );
    if let Some(route) = route
        && let Some(route_plugins) = route.plugins.as_ref()
    {
        plugins.extend(engine_bindings(&route_plugins.items));
    }

    let rate_limit = layer_rate_limit(
        upstream.rate_limit.as_ref(),
        route.and_then(|route| route.rate_limit.as_ref()),
    );
    let cors = layer_cors(
        upstream.cors.as_ref(),
        route.and_then(|route| route.cors.as_ref()),
    );

    let auth = upstream.auth.as_ref().map(|auth| AuthBinding {
        plugin_type: auth.plugin_type.clone(),
        config: auth.config.clone().unwrap_or(serde_json::Value::Null),
    });

    EffectiveConfig {
        guards: plugins_of(&plugins, crate::domain::model::PluginKind::Guard),
        transforms: plugins_of(&plugins, crate::domain::model::PluginKind::Transform),
        plugins,
        auth,
        rate_limit,
        cors,
        headers: upstream.headers.clone(),
    }
}

/// The upstream-relative path a matched route forwards to.
#[must_use]
pub fn matched_upstream_path(route: &Route, path: &str) -> String {
    match &route.matcher {
        RouteMatcher::Http(http_matcher) => upstream_path(http_matcher, path),
        RouteMatcher::Grpc(_) => path.to_owned(),
    }
}

fn plugins_of(
    plugins: &[PluginBinding],
    kind: crate::domain::model::PluginKind,
) -> Vec<PluginBinding> {
    plugins
        .iter()
        .filter(|binding| is_kind(&binding.plugin_ref, kind))
        .cloned()
        .collect()
}

/// The configuration document of a management-plane plugin binding.
fn binding_config(config: Option<serde_json::Value>) -> serde_json::Value {
    config.unwrap_or(serde_json::Value::Null)
}

/// Convert a management-plane chain into the engine's binding shape.
fn engine_bindings(
    items: &[crate::domain::model::PluginBinding],
) -> Vec<crate::infra::plugin::PluginBinding> {
    items
        .iter()
        .map(|binding| crate::infra::plugin::PluginBinding {
            plugin_ref: binding.plugin_ref.clone(),
            config: binding_config(binding.config.clone()),
        })
        .collect()
}

/// Resolve the plugin family of a reference, preferring the catalogue.
fn is_kind(plugin_ref: &str, kind: crate::domain::model::PluginKind) -> bool {
    crate::domain::services::management::built_in_plugin(plugin_ref)
        .is_none_or(|entry| entry.kind == kind)
}

/// `docs/ADR/0003` inheritance: the parent's limit when the child declares
/// none, `min(parent, child)` when it does.
#[must_use]
pub fn layer_rate_limit(
    parent: Option<&RateLimitConfig>,
    child: Option<&RateLimitConfig>,
) -> Option<RateLimitConfig> {
    match (parent, child) {
        (None, None) => None,
        (None, Some(child)) => Some(child.clone()),
        (Some(parent), None) => Some(parent.clone()),
        (Some(parent), Some(child)) => {
            let burst = match (parent.burst, child.burst) {
                (Some(parent_capacity), Some(child_capacity)) => Some(BurstConfig {
                    capacity: parent_capacity.capacity.min(child_capacity.capacity),
                }),
                (Some(parent_capacity), None) => Some(parent_capacity),
                (None, Some(child_capacity)) => Some(child_capacity),
                (None, None) => None,
            };
            Some(RateLimitConfig {
                sustained: min_sustained(&parent.sustained, &child.sustained),
                burst,
                ..child.clone()
            })
        }
    }
}

/// Lower sustained rate of the two layers, compared per second.
fn min_sustained(parent: &SustainedRate, child: &SustainedRate) -> SustainedRate {
    if per_second(&child.rate, &child.window) <= per_second(&parent.rate, &parent.window) {
        *child
    } else {
        *parent
    }
}

fn per_second(rate: &u32, window: &crate::domain::model::RateWindow) -> f64 {
    let seconds = window.seconds();
    if seconds == 0 {
        return f64::INFINITY;
    }
    f64::from(*rate) / f64::from(seconds as u32)
}

/// `docs/ADR/0004` CORS inheritance: origins and methods union, credentials
/// narrowed, `enabled` widened.
#[must_use]
pub fn layer_cors(parent: Option<&CorsConfig>, child: Option<&CorsConfig>) -> Option<CorsConfig> {
    match (parent, child) {
        (None, None) => None,
        (None, Some(child)) => Some(child.clone()),
        (Some(parent), None) => Some(parent.clone()),
        (Some(parent), Some(child)) => Some(CorsConfig {
            sharing: child.sharing,
            enabled: parent.enabled || child.enabled,
            allowed_origins: union(&parent.allowed_origins, &child.allowed_origins),
            allowed_methods: union(&parent.allowed_methods, &child.allowed_methods),
            expose_headers: union(&parent.expose_headers, &child.expose_headers),
            allow_credentials: parent.allow_credentials && child.allow_credentials,
        }),
    }
}

fn union(left: &[String], right: &[String]) -> Vec<String> {
    let mut merged = left.to_vec();
    for value in right {
        if !merged.contains(value) {
            merged.push(value.clone());
        }
    }
    merged
}

/// A failure for "no route matched".
#[must_use]
pub fn route_not_found(alias: &str, method: &str, path: &str) -> ProxyFailure {
    ProxyFailure::new(
        404,
        crate::domain::plugin::ROUTE_NOT_FOUND,
        "Route Not Found",
        format!("no enabled route of upstream '{alias}' matches {method} {path}"),
    )
}

/// A failure for "the alias resolved to a disabled upstream".
#[must_use]
pub fn upstream_unavailable(alias: &str) -> ProxyFailure {
    ProxyFailure::new(
        503,
        crate::domain::plugin::LINK_UNAVAILABLE,
        "Link Unavailable",
        format!("upstream '{alias}' is disabled"),
    )
}

#[cfg(test)]
#[path = "route_tests.rs"]
mod tests;
