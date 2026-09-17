//! Hierarchical configuration merge (`DESIGN.md` §3.1 "Hierarchical
//! Configuration" and ADR-0003).
//!
//! A resolution walks the tenant chain from the requesting tenant to the root
//! and collects every upstream that carries the requested alias. The chain is
//! ordered descendant-first; `merge` folds it back towards the root so that
//! ancestor constraints (`sharing: enforce`) are never bypassed by shadowing.

use std::collections::BTreeMap;

use crate::domain::model::{
    AuthConfig, CorsConfig, HeadersConfig, Passthrough, PluginBinding, RateLimitConfig, RouteSpec,
    Sharing, UpstreamSpec,
};

/// Effective configuration for a proxied request.
#[derive(Debug, Clone, Default)]
pub struct EffectiveConfig {
    /// Whether the resolved upstream serves traffic.
    pub enabled: bool,
    /// Union of ancestor and descendant tags.
    pub tags: BTreeMap<String, String>,
    /// Auth plugin binding that wins after the chain is folded.
    pub auth: Option<AuthConfig>,
    /// Header transformation rules after per-key folding.
    pub headers: HeadersConfig,
    /// Plugin chain: ancestor plugins first, descendant plugins last.
    pub plugins: Vec<PluginBinding>,
    /// Stricterst rate limit across the chain and the route.
    pub rate_limit: Option<RateLimitConfig>,
    /// CORS configuration after origin unioning.
    pub cors: Option<CorsConfig>,
}

impl EffectiveConfig {
    /// Effective tags as an ordered list.
    #[must_use]
    pub fn tag_list(&self) -> Vec<String> {
        self.tags.keys().cloned().collect()
    }
}

/// Merge an upstream chain (descendant first) with an optional route spec.
#[must_use]
pub fn merge(chain: &[&UpstreamSpec], route: Option<&RouteSpec>) -> EffectiveConfig {
    let mut effective = EffectiveConfig::default();
    for spec in chain.iter().rev() {
        // Fold from the root inwards: the accumulated value is the parent's.
        effective.enabled = spec.enabled;
        for tag in &spec.tags {
            effective.tags.insert(tag.clone(), String::new());
        }
        effective.headers = merge_headers(&effective.headers, &spec.headers);
        effective.auth = merge_auth(effective.auth.as_ref(), spec.auth.as_ref());
        effective.rate_limit = fold_rate_limit(effective.rate_limit, spec.rate_limit);
        effective.plugins = merge_plugins(&effective.plugins, &spec.plugins.items);
        effective.cors = fold_cors(effective.cors, spec.cors.clone());
    }
    // The closest upstream decides whether the link is live; tags are unioned
    // across the whole chain and can never be dropped by a descendant.
    if let Some(closest) = chain.first() {
        effective.enabled = closest.enabled;
        for tag in &closest.tags {
            effective.tags.insert(tag.clone(), String::new());
        }
    }
    if let Some(route) = route {
        effective.plugins.extend(route.plugins.items.clone());
        effective.rate_limit = fold_rate_limit(effective.rate_limit, route.rate_limit);
        effective.cors = fold_cors(effective.cors, route.cors.clone());
    }
    effective
}

/// Fold header rules: the descendant's rule wins per action.
#[must_use]
pub fn merge_headers(parent: &HeadersConfig, child: &HeadersConfig) -> HeadersConfig {
    let mut merged = child.clone();
    if merged.request.set.is_empty() {
        merged.request.set = parent.request.set.clone();
    }
    if merged.request.add.is_empty() {
        merged.request.add = parent.request.add.clone();
    }
    if merged.request.remove.is_empty() {
        merged.request.remove.clone_from(&parent.request.remove);
    }
    if merged.request.passthrough == Passthrough::None {
        merged.request.passthrough = parent.request.passthrough;
    }
    if merged.request.passthrough_allowlist.is_empty() {
        merged
            .request
            .passthrough_allowlist
            .clone_from(&parent.request.passthrough_allowlist);
    }
    if merged.response.set.is_empty() {
        merged.response.set = parent.response.set.clone();
    }
    if merged.response.add.is_empty() {
        merged.response.add = parent.response.add.clone();
    }
    if merged.response.remove.is_empty() {
        merged.response.remove.clone_from(&parent.response.remove);
    }
    merged
}

/// Fold `child`'s rate limit into the parent's (ADR-0003 inheritance table).
///
/// A `private` parent never contributes to its descendants, so the child's
/// own limit is used verbatim; `inherit` and `enforce` both cap the child at
/// `min(parent, child)`.
#[must_use]
pub fn fold_rate_limit(
    parent: Option<RateLimitConfig>,
    child: Option<RateLimitConfig>,
) -> Option<RateLimitConfig> {
    match (parent, child) {
        (Some(parent), child) if parent.sharing == Sharing::Private => child,
        (Some(parent), Some(child)) => Some(min_rate_limit(&parent, &child)),
        (Some(parent), None) => Some(parent),
        (None, child) => child,
    }
}

/// Component-wise `min` of two limits: the stricter always wins.
#[must_use]
pub fn min_rate_limit(left: &RateLimitConfig, right: &RateLimitConfig) -> RateLimitConfig {
    let (strict, relaxed) = if left.tokens_per_second() <= right.tokens_per_second() {
        (left, right)
    } else {
        (right, left)
    };
    RateLimitConfig {
        sharing: strict.sharing,
        algorithm: strict.algorithm,
        sustained: strict.sustained,
        burst: strict.burst.or(relaxed.burst),
        scope: strict.scope,
        strategy: strict.strategy,
        cost: strict.cost.max(relaxed.cost),
        response_headers: strict.response_headers && relaxed.response_headers,
    }
}

/// Fold `child`'s CORS configuration into the parent's (ADR-0004).
#[must_use]
pub fn fold_cors(parent: Option<CorsConfig>, child: Option<CorsConfig>) -> Option<CorsConfig> {
    match (parent, child) {
        (Some(parent), Some(child)) => match parent.sharing {
            Sharing::Enforce => Some(parent),
            Sharing::Inherit => Some(unioned(&parent, &child)),
            Sharing::Private => Some(child),
        },
        (Some(parent), None) => Some(parent),
        (None, child) => child,
    }
}

fn unioned(parent: &CorsConfig, child: &CorsConfig) -> CorsConfig {
    let mut merged = child.clone();
    for origin in &parent.allowed_origins {
        if !merged.allowed_origins.contains(origin) {
            merged.allowed_origins.push(origin.clone());
        }
    }
    for method in &parent.allowed_methods {
        if !merged.allowed_methods.contains(method) {
            merged.allowed_methods.push(method.clone());
        }
    }
    merged.enabled = parent.enabled || child.enabled;
    merged
}

/// Concatenate plugin chains: ancestor plugins first, descendant last.
#[must_use]
pub fn merge_plugins(parent: &[PluginBinding], child: &[PluginBinding]) -> Vec<PluginBinding> {
    let mut merged = parent.to_vec();
    merged.extend_from_slice(child);
    merged
}

/// Fold `child`'s auth binding into the parent's.
///
/// An `enforce` parent pins its own binding; otherwise the child wins when it
/// has one.
#[must_use]
pub fn merge_auth(parent: Option<&AuthConfig>, child: Option<&AuthConfig>) -> Option<AuthConfig> {
    match (parent, child) {
        (Some(parent), Some(child)) => match parent.sharing {
            Sharing::Enforce => Some(parent.clone()),
            Sharing::Inherit | Sharing::Private => Some(child.clone()),
        },
        (Some(parent), None) => Some(parent.clone()),
        (None, child) => child.cloned(),
    }
}

#[cfg(test)]
#[path = "merge_tests.rs"]
mod merge_tests;
