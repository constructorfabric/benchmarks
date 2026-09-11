//! Hierarchical configuration merge.
//!
//! Two independent axes combine here:
//!
//! * **Layering** (`cpt-cf-oagw-fr-config-layering`) — Upstream (base) <
//!   Route < Tenant.
//! * **Sharing** (`cpt-cf-oagw-fr-hierarchical-config`) — `private` hides a
//!   field from descendants, `inherit` lets them override it, `enforce` pins
//!   it. Rate limits additionally clamp to the stricter of the two, and
//!   enforced ancestor limits survive alias shadowing.

use crate::domain::model::{
    AuthConfig, CorsConfig, HeadersConfig, PluginBinding, RateLimitConfig, Route, SharingMode,
    Upstream,
};

/// The configuration a single proxy request actually executes under.
#[derive(Debug, Clone, Default)]
pub struct EffectiveConfig {
    pub auth: Option<AuthConfig>,
    pub headers: HeadersConfig,
    /// Upstream-bound bindings first, then route-bound ones.
    pub plugins: Vec<PluginBinding>,
    pub rate_limit: Option<RateLimitConfig>,
    pub cors: Option<CorsConfig>,
    pub tags: Vec<String>,
}

/// Merge the selected upstream, its matched route and the ancestor upstreams
/// that share the alias (ordered parent → root).
#[must_use]
pub fn effective_config(
    selected: &Upstream,
    ancestors: &[Upstream],
    route: Option<&Route>,
) -> EffectiveConfig {
    EffectiveConfig {
        auth: merge_auth(selected, ancestors),
        headers: selected.headers.clone().unwrap_or_default(),
        plugins: merge_plugins(selected, ancestors, route),
        rate_limit: merge_rate_limit(selected, ancestors, route),
        cors: merge_cors(selected, ancestors, route),
        tags: merge_tags(selected, ancestors, route),
    }
}

/// An ancestor with `sharing: enforce` pins the credential; otherwise the
/// closest configured auth wins, falling back to a visible ancestor's.
fn merge_auth(selected: &Upstream, ancestors: &[Upstream]) -> Option<AuthConfig> {
    // Root-most enforcement wins: walk from the root down so the outermost
    // `enforce` is the last one applied.
    if let Some(enforced) = ancestors
        .iter()
        .rev()
        .filter_map(|a| a.auth.as_ref())
        .find(|auth| auth.sharing.is_enforced())
    {
        return Some(enforced.clone());
    }
    if let Some(own) = selected.auth.as_ref().filter(|a| a.plugin_type.is_some()) {
        return Some(own.clone());
    }
    ancestors
        .iter()
        .filter_map(|a| a.auth.as_ref())
        .find(|auth| auth.sharing.is_visible_to_descendants() && auth.plugin_type.is_some())
        .cloned()
}

/// Ancestor chains prepend (root-most first), then the selected upstream's,
/// then the route's. Enforced ancestor plugins can never be dropped.
fn merge_plugins(
    selected: &Upstream,
    ancestors: &[Upstream],
    route: Option<&Route>,
) -> Vec<PluginBinding> {
    let mut chain: Vec<PluginBinding> = Vec::new();
    for ancestor in ancestors.iter().rev() {
        if let Some(plugins) = ancestor.plugins.as_ref()
            && plugins.sharing.is_visible_to_descendants()
        {
            chain.extend(plugins.items.iter().cloned());
        }
    }
    if let Some(plugins) = selected.plugins.as_ref() {
        chain.extend(plugins.items.iter().cloned());
    }
    if let Some(plugins) = route.and_then(|r| r.plugins.as_ref()) {
        chain.extend(plugins.items.iter().cloned());
    }
    chain
}

/// Route overrides upstream; every enforced ancestor limit then clamps the
/// result to the stricter value.
fn merge_rate_limit(
    selected: &Upstream,
    ancestors: &[Upstream],
    route: Option<&Route>,
) -> Option<RateLimitConfig> {
    let mut effective = route
        .and_then(|r| r.rate_limit)
        .or(selected.rate_limit);

    for ancestor in ancestors {
        let Some(limit) = ancestor.rate_limit else {
            continue;
        };
        if !limit.sharing.is_visible_to_descendants() {
            continue;
        }
        effective = Some(match effective {
            // An `inherit` ancestor limit only applies when the descendant
            // states none; an `enforce` one always clamps.
            Some(own) if limit.sharing.is_enforced() => RateLimitConfig::stricter_of(own, limit),
            Some(own) => own,
            None => limit,
        });
    }
    effective
}

/// `enforce` pins the ancestor policy, `inherit` unions the origin lists, and
/// a route-level policy overrides the upstream's.
fn merge_cors(
    selected: &Upstream,
    ancestors: &[Upstream],
    route: Option<&Route>,
) -> Option<CorsConfig> {
    if let Some(enforced) = ancestors
        .iter()
        .rev()
        .filter_map(|a| a.cors.as_ref())
        .find(|cors| cors.sharing.is_enforced())
    {
        return Some(enforced.clone());
    }

    let mut effective = route
        .and_then(|r| r.cors.clone())
        .or_else(|| selected.cors.clone());

    for ancestor in ancestors {
        let Some(inherited) = ancestor.cors.as_ref() else {
            continue;
        };
        if inherited.sharing != SharingMode::Inherit {
            continue;
        }
        effective = Some(match effective {
            Some(mut own) => {
                for origin in &inherited.allowed_origins {
                    if !own.allowed_origins.contains(origin) {
                        own.allowed_origins.push(origin.clone());
                    }
                }
                own
            }
            None => inherited.clone(),
        });
    }
    effective
}

/// Tags have no sharing mode: they always union, ancestors first.
fn merge_tags(selected: &Upstream, ancestors: &[Upstream], route: Option<&Route>) -> Vec<String> {
    let mut tags: Vec<String> = Vec::new();
    let mut push = |candidates: &[String]| {
        for tag in candidates {
            if !tags.contains(tag) {
                tags.push(tag.clone());
            }
        }
    };
    for ancestor in ancestors.iter().rev() {
        push(&ancestor.tags);
    }
    push(&selected.tags);
    if let Some(route) = route {
        push(&route.tags);
    }
    tags
}

#[cfg(test)]
#[path = "merge_tests.rs"]
mod tests;
