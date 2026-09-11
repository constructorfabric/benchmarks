//! Alias shadowing, route matching and effective-configuration merge.
//!
//! This is the single tenant-hierarchy walk described in ADR-0006: resolve the
//! alias descendant → root, take the closest match as the routing target,
//! then fold in the ancestors' `inherit` / `enforce` constraints so shadowing
//! can never bypass them.

use crate::domain::error::OagwError;
use crate::domain::model::{
    AuthConfig, CorsConfig, HeadersConfig, HttpMatch, PathSuffixMode, PluginBinding,
    RateLimitConfig, Route, SharingMode, Upstream,
};

/// The configuration actually applied to a proxied request.
#[derive(Debug, Clone, Default)]
pub struct EffectiveConfig {
    /// `false` when the selected upstream, or any same-alias ancestor, is
    /// disabled.
    pub enabled: bool,
    /// Auth binding after `enforce` / override resolution.
    pub auth: Option<AuthConfig>,
    /// Header transformation rules (upstream-level only).
    pub headers: Option<HeadersConfig>,
    /// Ordered plugin chain: ancestors, then the selected upstream, then the
    /// matched route.
    pub plugins: Vec<PluginBinding>,
    /// The strictest rate limit across the whole chain.
    pub rate_limit: Option<RateLimitConfig>,
    /// CORS policy after union / enforce resolution.
    pub cors: Option<CorsConfig>,
    /// Add-only union of tags across the chain.
    pub tags: Vec<String>,
}

/// Where a matched route came from.
#[derive(Debug, Clone)]
pub struct MatchedRoute {
    /// The route itself.
    pub route: Route,
    /// The outbound path after applying `path_suffix_mode`.
    pub outbound_path: String,
}

/// Result of resolving `{alias}` against a tenant chain.
#[derive(Debug, Clone)]
pub struct ResolvedUpstream {
    /// The routing target — the closest same-alias upstream in the chain.
    pub selected: Upstream,
    /// Same-alias upstreams in ancestor tenants, closest ancestor first.
    pub ancestors: Vec<Upstream>,
}

impl ResolvedUpstream {
    /// Pick the routing target out of an already ordered (descendant → root)
    /// candidate list.
    #[must_use]
    pub fn from_chain(mut candidates: Vec<Upstream>) -> Option<Self> {
        if candidates.is_empty() {
            return None;
        }
        let selected = candidates.remove(0);
        Some(Self {
            selected,
            ancestors: candidates,
        })
    }

    /// `false` when the target or any same-alias ancestor is disabled —
    /// an ancestor's disable is not re-enableable by a descendant.
    #[must_use]
    pub fn effectively_enabled(&self) -> bool {
        self.selected.spec.enabled && self.ancestors.iter().all(|u| u.spec.enabled)
    }
}

/// Normalize the raw inbound path suffix into a leading-slash form.
#[must_use]
pub fn normalize_suffix(raw: Option<&str>) -> String {
    let raw = raw.unwrap_or("").trim();
    if raw.is_empty() || raw == "/" {
        return "/".to_owned();
    }
    if raw.starts_with('/') {
        raw.to_owned()
    } else {
        format!("/{raw}")
    }
}

/// Match `suffix` against a route path prefix, returning the remaining
/// suffix. A route path of `/` matches everything.
#[must_use]
pub fn match_path<'a>(route_path: &str, suffix: &'a str) -> Option<&'a str> {
    let base = route_path.trim_end_matches('/');
    if base.is_empty() {
        return Some(suffix);
    }
    if suffix == base {
        return Some("");
    }
    suffix
        .strip_prefix(base)
        .filter(|rest| rest.starts_with('/'))
}

/// Compose the outbound path for a matched route.
#[must_use]
pub fn outbound_path(route_path: &str, remaining: &str) -> String {
    let base = route_path.trim_end_matches('/');
    let composed = format!("{base}{remaining}");
    if composed.is_empty() {
        "/".to_owned()
    } else {
        composed
    }
}

/// The method a route's `methods` list is matched against.
///
/// `HEAD` is a `GET` without a response body (RFC 9110 §9.3.2) and the route
/// schema's method enum cannot express it, so a `GET` route serves it; the
/// request is still forwarded as `HEAD`.
#[must_use]
pub fn match_method(method: &str) -> String {
    if method.eq_ignore_ascii_case("HEAD") {
        "GET".to_owned()
    } else {
        method.to_ascii_uppercase()
    }
}

/// Select the best HTTP route for `(method, suffix)` out of one hierarchy
/// level: longest matching path wins, then highest `priority`.
#[must_use]
pub fn best_route_at_level(
    routes: &[Route],
    method: &str,
    suffix: &str,
) -> Option<(Route, String)> {
    let method = match_method(method);
    let mut best: Option<(&Route, &HttpMatch, &str)> = None;
    for route in routes {
        if !route.spec.enabled {
            continue;
        }
        let Some(http) = route.http() else { continue };
        if !http.methods.iter().any(|m| m.eq_ignore_ascii_case(&method)) {
            continue;
        }
        let Some(remaining) = match_path(&http.path, suffix) else {
            continue;
        };
        let better = match best {
            None => true,
            Some((current, current_http, _)) => {
                let new_len = http.path.trim_end_matches('/').len();
                let cur_len = current_http.path.trim_end_matches('/').len();
                new_len > cur_len
                    || (new_len == cur_len && route.spec.priority > current.spec.priority)
            }
        };
        if better {
            best = Some((route, http, remaining));
        }
    }
    best.map(|(route, _, remaining)| (route.clone(), remaining.to_owned()))
}

/// Match a route across the tenant chain: level order decides first (a
/// descendant's route beats an ancestor's), then longest path and priority
/// within the level.
#[must_use]
pub fn match_route_in_chain(
    levels: &[Vec<Route>],
    method: &str,
    suffix: &str,
) -> Option<(Route, String)> {
    levels
        .iter()
        .find_map(|routes| best_route_at_level(routes, method, suffix))
}

/// Apply `path_suffix_mode` and produce the outbound path.
///
/// # Errors
///
/// Returns `400` when a suffix is supplied to a route that disables it.
pub fn apply_path_suffix(http: &HttpMatch, remaining: &str) -> Result<String, OagwError> {
    match http.path_suffix_mode {
        PathSuffixMode::Disabled if !remaining.is_empty() && remaining != "/" => {
            Err(OagwError::validation(format!(
                "route '{}' has path_suffix_mode 'disabled' but a path suffix '{remaining}' was \
                 supplied",
                http.path
            )))
        }
        PathSuffixMode::Disabled => Ok(outbound_path(&http.path, "")),
        PathSuffixMode::Append => Ok(outbound_path(&http.path, remaining)),
    }
}

/// Validate inbound query parameters against the route allowlist.
///
/// An empty allowlist allows none, per the route schema.
///
/// # Errors
///
/// Returns `400` naming the first parameter that is not allowed.
pub fn filter_query(
    http: &HttpMatch,
    query: &[(String, String)],
) -> Result<Vec<(String, String)>, OagwError> {
    let mut allowed = Vec::with_capacity(query.len());
    for (name, value) in query {
        if http.query_allowlist.iter().any(|a| a == name) {
            allowed.push((name.clone(), value.clone()));
        } else {
            return Err(OagwError::validation(format!(
                "query parameter '{name}' is not in the route's query_allowlist"
            )));
        }
    }
    Ok(allowed)
}

/// Pick the strictest of two rate limits: `min` on both the per-second
/// sustained rate and the burst capacity, keeping the more specific block's
/// scope, strategy and cost.
#[must_use]
pub fn strictest(
    specific: Option<RateLimitConfig>,
    broader: Option<&RateLimitConfig>,
) -> Option<RateLimitConfig> {
    match (specific, broader) {
        (None, None) => None,
        (Some(specific), None) => Some(specific),
        (None, Some(broader)) => Some(broader.clone()),
        (Some(mut specific), Some(broader)) => {
            if broader.refill_per_second() < specific.refill_per_second() {
                specific.sustained = broader.sustained;
            }
            let capacity = specific.capacity().min(broader.capacity());
            specific.burst = Some(crate::domain::model::BurstRate {
                capacity: Some(capacity),
            });
            Some(specific)
        }
    }
}

/// Merge the selected upstream, its same-alias ancestors and the matched
/// route into the configuration actually applied.
#[must_use]
pub fn effective_config(resolved: &ResolvedUpstream, route: Option<&Route>) -> EffectiveConfig {
    let selected = &resolved.selected;

    // Auth: the closest ancestor with `enforce` wins outright; otherwise the
    // selected upstream's own binding, falling back to the closest visible
    // (`inherit`) ancestor binding when the selected upstream declares none.
    let enforced_auth = resolved
        .ancestors
        .iter()
        .find(|u| {
            u.spec
                .auth
                .as_ref()
                .is_some_and(|a| a.sharing == SharingMode::Enforce)
        })
        .and_then(|u| u.spec.auth.clone());
    let inherited_auth = resolved
        .ancestors
        .iter()
        .find(|u| {
            u.spec
                .auth
                .as_ref()
                .is_some_and(|a| a.sharing != SharingMode::Private)
        })
        .and_then(|u| u.spec.auth.clone());
    let auth = enforced_auth
        .or_else(|| selected.spec.auth.clone())
        .or(inherited_auth);

    // Plugins: ancestors (root first) then the selected upstream then the
    // route. A `private` ancestor chain is invisible to descendants.
    let mut plugins: Vec<PluginBinding> = Vec::new();
    for ancestor in resolved.ancestors.iter().rev() {
        if let Some(cfg) = &ancestor.spec.plugins
            && cfg.sharing != SharingMode::Private
        {
            plugins.extend(cfg.items.iter().cloned());
        }
    }
    if let Some(cfg) = &selected.spec.plugins {
        plugins.extend(cfg.items.iter().cloned());
    }
    if let Some(cfg) = route.and_then(|r| r.spec.plugins.as_ref()) {
        plugins.extend(cfg.items.iter().cloned());
    }

    // Rate limits: route < upstream < every enforcing ancestor, `min` all the
    // way, so shadowing cannot loosen an ancestor's enforced ceiling.
    let mut rate_limit = strictest(
        route.and_then(|r| r.spec.rate_limit.clone()),
        selected.spec.rate_limit.as_ref(),
    );
    for ancestor in &resolved.ancestors {
        if let Some(limit) = &ancestor.spec.rate_limit
            && limit.sharing == SharingMode::Enforce
        {
            rate_limit = strictest(rate_limit, Some(limit));
        }
    }

    // CORS: a route override is the most specific; an enforcing ancestor
    // replaces it; an `inherit` ancestor unions its origins in.
    let mut cors = route
        .and_then(|r| r.spec.cors.clone())
        .or_else(|| selected.spec.cors.clone());
    for ancestor in &resolved.ancestors {
        let Some(ancestor_cors) = &ancestor.spec.cors else {
            continue;
        };
        match ancestor_cors.sharing {
            SharingMode::Enforce => cors = Some(ancestor_cors.clone()),
            SharingMode::Inherit => {
                cors = Some(match cors {
                    None => ancestor_cors.clone(),
                    Some(mut own) => {
                        for origin in &ancestor_cors.allowed_origins {
                            if !own.allowed_origins.contains(origin) {
                                own.allowed_origins.push(origin.clone());
                            }
                        }
                        own.enabled = own.enabled || ancestor_cors.enabled;
                        own
                    }
                });
            }
            SharingMode::Private => {}
        }
    }

    // Tags are add-only across the hierarchy: no sharing mode, no removal.
    let mut tags: Vec<String> = Vec::new();
    for ancestor in resolved.ancestors.iter().rev() {
        tags.extend(ancestor.spec.tags.iter().cloned());
    }
    tags.extend(selected.spec.tags.iter().cloned());
    if let Some(route) = route {
        tags.extend(route.spec.tags.iter().cloned());
    }
    tags.sort();
    tags.dedup();

    EffectiveConfig {
        enabled: resolved.effectively_enabled(),
        auth,
        headers: selected.spec.headers.clone(),
        plugins,
        rate_limit,
        cors,
        tags,
    }
}

#[cfg(test)]
#[path = "resolve_tests.rs"]
mod tests;
