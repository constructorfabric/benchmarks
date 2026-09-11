//! Configuration merge: upstream → route → tenant.
//!
//! Each later layer narrows or appends to the earlier one. Sharing modes
//! decide whether a descendant may override an ancestor's value.

use crate::domain::error::DomainError;
use crate::domain::model::{
    Cors, HeadersConfig, PluginRef, PluginsConfig, RateLimit, SharingMode, SustainedRate, Upstream,
};
use std::collections::BTreeMap;

/// One entry of the effective plugin chain.
#[derive(Debug, Clone, PartialEq)]
pub struct PluginBinding {
    /// The plugin identifier the registry resolves.
    pub id: String,
    /// The configuration the plugin reads.
    pub config: serde_json::Value,
}

impl PluginBinding {
    /// Builds a binding for an unconfigured plugin.
    #[must_use]
    pub fn bare(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            config: serde_json::Value::Null,
        }
    }
}

/// Effective configuration for a proxy request, after merging.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct EffectiveConfig {
    /// Plugins, upstream items then route items.
    pub plugins: Vec<PluginBinding>,
    /// Effective sustained rate, the `min` across layers.
    pub sustained_rate: Option<u32>,
    /// Effective burst capacity, the `min` across layers.
    pub burst_capacity: Option<u32>,
    /// Effective rate-limit window, taken from the strictest layer.
    pub sustained_window: Option<crate::domain::model::RateWindow>,
    /// Rate-limit scope of the strictest layer.
    pub rate_scope: Option<crate::domain::model::RateScope>,
    /// Cost per request.
    pub rate_cost: u32,
    /// CORS origins, unioned across layers.
    pub cors_origins: Vec<String>,
    /// CORS methods, unioned across layers.
    pub cors_methods: Vec<String>,
    /// Whether CORS processing is enabled on any layer.
    pub cors_enabled: bool,
    /// Whether credentials are allowed by CORS.
    pub cors_allow_credentials: bool,
    /// Headers to expose to the browser.
    pub cors_expose_headers: Vec<String>,
    /// Tags, add-only union.
    pub tags: Vec<String>,
}

/// Validates a rate limit configuration.
///
/// # Errors
///
/// Returns a message when the sustained rate is zero.
pub fn validate_rate_limit(rate_limit: &RateLimit) -> Result<(), DomainError> {
    if rate_limit.sustained.rate == 0 {
        return Err(DomainError::Invalid(
            "rate_limit.sustained.rate must be at least 1".to_owned(),
        ));
    }
    if rate_limit.cost == 0 {
        return Err(DomainError::Invalid(
            "rate_limit.cost must be at least 1".to_owned(),
        ));
    }
    Ok(())
}

/// Validates a CORS configuration.
///
/// # Errors
///
/// Returns a message when credentials are combined with a wildcard origin.
pub fn validate_cors(cors: &Cors) -> Result<(), DomainError> {
    if cors.allow_credentials && cors.allowed_origins.iter().any(|origin| origin == "*") {
        return Err(DomainError::Invalid(
            "cors.allow_credentials cannot be combined with allowed_origins ['*']".to_owned(),
        ));
    }
    for origin in &cors.allowed_origins {
        if origin == "*" {
            continue;
        }
        if !origin.starts_with("http://") && !origin.starts_with("https://") {
            return Err(DomainError::Invalid(format!(
                "cors origin '{origin}' must be an absolute origin"
            )));
        }
    }
    Ok(())
}

/// Merges two rate limits, keeping the stricter values.
#[must_use]
pub fn merge_rate_limit(
    base: Option<&RateLimit>,
    override_: Option<&RateLimit>,
) -> Option<RateLimit> {
    match (base, override_) {
        (None, None) => None,
        (Some(only), None) | (None, Some(only)) => Some(only.clone()),
        (Some(base), Some(over)) => {
            let (strict_rate, strict_window) = if is_stricter(base, over) {
                (base.sustained.rate, base.sustained.window)
            } else {
                (over.sustained.rate, over.sustained.window)
            };
            let capacity = base.capacity().min(over.capacity());
            Some(RateLimit {
                sharing: if over.sharing == SharingMode::Enforce {
                    SharingMode::Enforce
                } else {
                    base.sharing
                },
                algorithm: over.algorithm,
                sustained: SustainedRate {
                    rate: strict_rate,
                    window: strict_window,
                },
                burst: crate::domain::model::Burst {
                    capacity: Some(capacity),
                },
                scope: over.scope,
                strategy: over.strategy,
                cost: over.cost,
            })
        }
    }
}

/// Whether `candidate` replenishes no faster than `other`, compared exactly by
/// cross-multiplying the rate fractions.
fn is_stricter(candidate: &RateLimit, other: &RateLimit) -> bool {
    let candidate_ns = candidate.sustained.window.duration().as_nanos().max(1);
    let other_ns = other.sustained.window.duration().as_nanos().max(1);
    let left = u128::from(candidate.sustained.rate) * other_ns;
    let right = u128::from(other.sustained.rate) * candidate_ns;
    left <= right
}

/// Concatenates plugin chains: upstream items then route items.
#[must_use]
pub fn merge_plugins(
    upstream: &PluginsConfig,
    route: Option<&PluginsConfig>,
) -> Vec<PluginBinding> {
    let mut items: Vec<PluginBinding> = upstream.items.iter().map(binding_of).collect::<Vec<_>>();
    if let Some(route) = route {
        items.extend(route.items.iter().map(binding_of));
    }
    items
}

/// Flattens one chain entry into the binding the data plane runs.
fn binding_of(item: &PluginRef) -> PluginBinding {
    PluginBinding {
        id: item.id(),
        config: item.config(),
    }
}

/// Unions CORS origins, keeping the strictest credential posture.
#[must_use]
pub fn merge_cors(upstream: Option<&Cors>, route: Option<&Cors>) -> Option<Cors> {
    match (upstream, route) {
        (None, None) => None,
        (Some(only), None) | (None, Some(only)) => Some(only.clone()),
        (Some(upstream), Some(route)) => {
            let mut merged = upstream.clone();
            merged.enabled = upstream.enabled || route.enabled;
            for origin in &route.allowed_origins {
                if !merged.allowed_origins.contains(origin) {
                    merged.allowed_origins.push(origin.clone());
                }
            }
            for method in &route.allowed_methods {
                if !merged.allowed_methods.contains(method) {
                    merged.allowed_methods.push(method.clone());
                }
            }
            merged.allow_credentials = upstream.allow_credentials || route.allow_credentials;
            Some(merged)
        }
    }
}

/// Unions tags additively.
#[must_use]
pub fn merge_tags(upstream: &[String], route: &[String]) -> Vec<String> {
    let mut tags: Vec<String> = Vec::new();
    for tag in upstream.iter().chain(route.iter()) {
        if !tags.contains(tag) {
            tags.push(tag.clone());
        }
    }
    tags
}

/// Builds the effective configuration for a request.
#[must_use]
pub fn effective(
    upstream: &Upstream,
    route: Option<&crate::domain::model::Route>,
) -> EffectiveConfig {
    let plugins = merge_plugins(&upstream.plugins, route.map(|r| &r.plugins));
    let rate_limit = merge_rate_limit(
        upstream.rate_limit.as_ref(),
        route.and_then(|r| r.rate_limit.as_ref()),
    );
    let cors = merge_cors(upstream.cors.as_ref(), route.and_then(|r| r.cors.as_ref()));
    let (sustained_rate, sustained_window, burst_capacity, rate_scope, rate_cost) = match rate_limit
    {
        Some(ref limit) => (
            Some(limit.sustained.rate),
            Some(limit.sustained.window),
            Some(limit.capacity()),
            Some(limit.scope),
            limit.cost,
        ),
        None => (None, None, None, None, 1),
    };
    let Some(cors) = cors.as_ref() else {
        return EffectiveConfig {
            plugins,
            sustained_rate,
            burst_capacity,
            sustained_window,
            rate_scope,
            rate_cost,
            cors_origins: Vec::new(),
            cors_methods: Vec::new(),
            cors_enabled: false,
            cors_allow_credentials: false,
            cors_expose_headers: Vec::new(),
            tags: merge_tags(
                &upstream.tags,
                route.map_or(&[], |route| route.tags.as_slice()),
            ),
        };
    };
    EffectiveConfig {
        plugins,
        sustained_rate,
        burst_capacity,
        sustained_window,
        rate_scope,
        rate_cost,
        cors_origins: cors.allowed_origins.clone(),
        cors_methods: cors.allowed_methods.clone(),
        cors_enabled: cors.enabled,
        cors_allow_credentials: cors.allow_credentials,
        cors_expose_headers: cors.expose_headers.clone(),
        tags: merge_tags(
            &upstream.tags,
            route.map_or(&[], |route| route.tags.as_slice()),
        ),
    }
}

/// Merges two header rule sets, the later layer adding to the earlier.
#[must_use]
pub fn merge_headers(
    upstream: &HeadersConfig,
    route_headers: Option<&HeadersConfig>,
) -> HeadersConfig {
    let mut merged = upstream.clone();
    if let Some(route) = route_headers {
        for (name, value) in &route.request.set {
            merged.request.set.insert(name.clone(), value.clone());
        }
        for (name, value) in &route.request.add {
            merged.request.add.insert(name.clone(), value.clone());
        }
        for name in &route.request.remove {
            merged.request.remove.push(name.clone());
        }
        if route.request.passthrough != crate::domain::model::PassthroughMode::None {
            merged.request.passthrough = route.request.passthrough;
            merged
                .request
                .passthrough_allowlist
                .clone_from(&route.request.passthrough_allowlist);
        }
        for (name, value) in &route.response.set {
            merged.response.set.insert(name.clone(), value.clone());
        }
        for (name, value) in &route.response.add {
            merged.response.add.insert(name.clone(), value.clone());
        }
        for name in &route.response.remove {
            merged.response.remove.push(name.clone());
        }
    }
    merged
}

/// Whether an ancestor's configuration may be overridden.
#[must_use]
pub fn may_override(mode: SharingMode) -> bool {
    matches!(mode, SharingMode::Inherit)
}

/// Whether an ancestor's configuration is forced onto descendants.
#[must_use]
pub fn is_enforced(mode: SharingMode) -> bool {
    matches!(mode, SharingMode::Enforce)
}

/// Normalises a set of headers into a sorted map (used by tests and DTOs).
#[must_use]
pub fn header_map(entries: &[(&str, &str)]) -> BTreeMap<String, String> {
    entries
        .iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .collect()
}

#[cfg(test)]
#[path = "merge_tests.rs"]
mod tests;
