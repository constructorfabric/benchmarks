//! Configuration layering: upstream < route < tenant.
//!
//! Each field merges independently; `SharingMode::Enforce` on an ancestor
//! layer makes that layer's value non-overridable, and tags are an add-only
//! union a descendant cannot shrink. Routes may override the rate limit and
//! add plugins; header rules and CORS come from the upstream (and, when
//! enforced, from ancestors).

use crate::domain::dto::{Cors, HeaderRules, PluginSet, RateLimit, Route, SharingMode, Upstream};
use crate::domain::ratelimit::EffectiveRateLimit;

/// Which layer a piece of effective configuration came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LayerOrigin {
    /// The upstream's own configuration.
    Upstream,
    /// The matched route's override.
    Route,
    /// An ancestor's enforced configuration.
    Ancestor,
}

/// The effective configuration of a proxied request.
#[derive(Debug, Clone)]
pub struct EffectiveConfig {
    /// Upstream id the request resolves to.
    pub upstream_id: String,
    /// Owning tenant of the selected upstream.
    pub owner_tenant_id: uuid::Uuid,
    /// The alias that resolved.
    pub alias: String,
    /// Whether the effective upstream is enabled.
    pub enabled: bool,
    /// Effective rate limit, `None` when no layer configures one.
    pub rate_limit: Option<EffectiveRateLimit>,
    /// Whether an ancestor enforced that rate limit.
    pub rate_limit_enforced: bool,
    /// Effective header rules.
    pub headers: Option<HeaderRules>,
    /// Effective plugin bindings, upstream plugins first.
    pub plugins: Vec<String>,
    /// Configuration a `{plugin_ref, config}` binding carries (ADR 0009),
    /// keyed by the reference it belongs to.
    pub plugin_configs: std::collections::BTreeMap<String, serde_json::Value>,
    /// Effective CORS policy.
    pub cors: Option<Cors>,
    /// Effective tags: the ancestor union plus the upstream's own.
    pub tags: Vec<String>,
}

impl Default for EffectiveConfig {
    fn default() -> Self {
        Self {
            upstream_id: String::new(),
            owner_tenant_id: uuid::Uuid::nil(),
            alias: String::new(),
            enabled: true,
            rate_limit: None,
            rate_limit_enforced: false,
            headers: None,
            plugins: Vec::new(),
            plugin_configs: std::collections::BTreeMap::new(),
            cors: None,
            tags: Vec::new(),
        }
    }
}

/// Whether a layer may be overridden given the owner's sharing mode.
pub fn is_overridable(owner: Option<SharingMode>) -> bool {
    !matches!(owner, Some(SharingMode::Enforce))
}

/// Folds a route's overrides onto the upstream's configuration.
pub fn fold_route(
    upstream: &Upstream,
    owner_tenant_id: uuid::Uuid,
    route: Option<&Route>,
) -> EffectiveConfig {
    let rate_limit = upstream.rate_limit.as_ref().map(EffectiveRateLimit::from_config);
    let mut config = EffectiveConfig {
        upstream_id: upstream.id.clone().unwrap_or_default(),
        owner_tenant_id,
        alias: upstream.alias_str().to_string(),
        enabled: upstream.enabled,
        rate_limit,
        rate_limit_enforced: upstream
            .rate_limit
            .as_ref()
            .map(|r| !is_overridable(r.sharing))
            .unwrap_or(false),
        headers: upstream.headers.clone(),
        plugins: upstream
            .plugins
            .as_ref()
            .map(|p| p.items.clone())
            .unwrap_or_default(),
        plugin_configs: upstream
            .plugins
            .as_ref()
            .map(|p| p.configs.clone())
            .unwrap_or_default(),
        cors: upstream.cors.clone(),
        tags: upstream.tags.clone(),
    };

    let Some(route) = route else {
        return config;
    };

    // Route rate-limit override, unless the upstream enforces its own.
    let route_limit = route.rate_limit.as_ref().map(EffectiveRateLimit::from_config);
    if is_overridable(upstream.rate_limit.as_ref().and_then(|r| r.sharing)) {
        if let Some(route_limit) = route_limit {
            let enforced = config.rate_limit_enforced;
            match &mut config.rate_limit {
                Some(existing) => existing.merge(&route_limit),
                None => config.rate_limit = Some(route_limit),
            }
            config.rate_limit_enforced = enforced;
        }
    }

    // Route plugins are appended after the upstream's own.
    if let Some(set) = route.plugins.as_ref() {
        for item in &set.items {
            if !config.plugins.contains(item) {
                config.plugins.push(item.clone());
            }
        }
        for (reference, bound) in &set.configs {
            config.plugin_configs.insert(reference.clone(), bound.clone());
        }
    }

    // Route tags join the union.
    for tag in &route.tags {
        if !config.tags.contains(tag) {
            config.tags.push(tag.clone());
        }
    }

    config
}

/// Merges an ancestor-enforced layer into the effective configuration.
///
/// Enforced ancestors constrain the rate limit (`min()`), can disable the
/// upstream for descendants, and add tags. A descendant can never relax them.
pub fn merge_ancestor_enforced(config: &mut EffectiveConfig, ancestor: &Upstream) {
    if let Some(ancestor_limit) = ancestor.rate_limit.as_ref() {
        if !is_overridable(ancestor_limit.sharing) {
            let layer = EffectiveRateLimit {
                ancestor_enforced: true,
                ..EffectiveRateLimit::from_config(ancestor_limit)
            };
            match &mut config.rate_limit {
                Some(existing) => existing.merge(&layer),
                None => config.rate_limit = Some(layer),
            }
            config.rate_limit_enforced = true;
        }
    }
    if !ancestor.enabled {
        config.enabled = false;
    }
    for tag in &ancestor.tags {
        if !config.tags.contains(tag) {
            config.tags.push(tag.clone());
        }
    }
}

/// Unions the tags of a set of layers, first-seen order preserved.
pub fn union_tags(layers: &[Vec<String>]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for layer in layers {
        for tag in layer {
            if !out.contains(tag) {
                out.push(tag.clone());
            }
        }
    }
    out
}

/// The plugin set after layering: upstream plugins first, then route plugins.
pub fn merge_plugins(upstream: Option<&PluginSet>, route: Option<&PluginSet>) -> Vec<String> {
    let mut out = Vec::new();
    for set in [upstream, route].into_iter().flatten() {
        for item in &set.items {
            if !out.contains(item) {
                out.push(item.clone());
            }
        }
    }
    out
}

/// Merges header rules, upstream rules first and route rules on top.
pub fn merge_headers(upstream: Option<&HeaderRules>, route: Option<&HeaderRules>) -> HeaderRules {
    let mut out = HeaderRules::default();
    if let Some(rules) = upstream {
        out.request = rules.request.clone();
        out.response = rules.response.clone();
    }
    if let Some(rules) = route {
        if rules.request.is_some() {
            out.request = rules.request.clone();
        }
        if rules.response.is_some() {
            out.response = rules.response.clone();
        }
    }
    out
}

/// Chooses the effective CORS policy, ancestor-enforced winning.
pub fn merge_cors(upstream: Option<&Cors>, ancestor: Option<&Cors>) -> Option<Cors> {
    match (ancestor, upstream) {
        (Some(a), _) if !is_overridable(a.sharing) => Some(a.clone()),
        (_, Some(u)) => Some(u.clone()),
        (Some(a), None) => Some(a.clone()),
        (None, None) => None,
    }
}

/// Re-applies a rate limit over the effective config (used by the data plane
/// when a tenant-level layer is discovered after route folding).
pub fn apply_rate_limit(config: &mut EffectiveConfig, limit: &RateLimit, enforced: bool) {
    let layer = EffectiveRateLimit {
        ancestor_enforced: enforced,
        ..EffectiveRateLimit::from_config(limit)
    };
    match &mut config.rate_limit {
        Some(existing) => existing.merge(&layer),
        None => config.rate_limit = Some(layer),
    }
    if enforced {
        config.rate_limit_enforced = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::dto::{PluginSet, RateWindow, Sustained};
    use std::collections::BTreeMap;

    fn limit(rate: u64, sharing: Option<SharingMode>) -> RateLimit {
        RateLimit {
            sharing,
            sustained: Sustained { rate, window: RateWindow::Second },
            ..RateLimit::default()
        }
    }

    fn upstream() -> Upstream {
        Upstream { id: Some("u".into()), ..Upstream::default() }
    }

    #[test]
    fn enforce_blocks_override_and_inherit_allows_it() {
        assert!(!is_overridable(Some(SharingMode::Enforce)));
        assert!(is_overridable(Some(SharingMode::Inherit)));
        assert!(is_overridable(Some(SharingMode::Private)));
        assert!(is_overridable(None));
    }

    #[test]
    fn the_route_rate_limit_overrides_the_upstream_one() {
        let mut u = upstream();
        u.rate_limit = Some(limit(100, None));
        let route = Route {
            rate_limit: Some(limit(10, None)),
            ..Route::default()
        };
        let effective = fold_route(&u, uuid::Uuid::nil(), Some(&route));
        assert_eq!(effective.rate_limit.as_ref().map(|r| r.capacity), Some(10));
    }

    #[test]
    fn an_enforced_upstream_rate_limit_cannot_be_relaxed() {
        let mut u = upstream();
        u.rate_limit = Some(limit(10, Some(SharingMode::Enforce)));
        let route = Route {
            rate_limit: Some(limit(1000, None)),
            ..Route::default()
        };
        let effective = fold_route(&u, uuid::Uuid::nil(), Some(&route));
        assert_eq!(effective.rate_limit.as_ref().map(|r| r.capacity), Some(10));
        assert!(effective.rate_limit_enforced);
    }

    #[test]
    fn an_ancestor_enforced_limit_lowered_the_effective_value() {
        let mut config = EffectiveConfig::default();
        config.rate_limit = Some(EffectiveRateLimit::from_config(&limit(100, None)));
        let mut ancestor = upstream();
        ancestor.rate_limit = Some(limit(5, Some(SharingMode::Enforce)));
        merge_ancestor_enforced(&mut config, &ancestor);
        assert_eq!(config.rate_limit.as_ref().map(|r| r.capacity), Some(5));
        assert!(config.rate_limit_enforced);
    }

    #[test]
    fn a_descendant_cannot_re_enable_an_ancestor_disabled_upstream() {
        let mut config = EffectiveConfig::default();
        config.enabled = true;
        let mut ancestor = upstream();
        ancestor.enabled = false;
        merge_ancestor_enforced(&mut config, &ancestor);
        assert!(!config.enabled);
    }

    #[test]
    fn tags_are_an_add_only_union() {
        let merged = union_tags(&[vec!["a".into(), "b".into()], vec!["b".into(), "c".into()]]);
        assert_eq!(merged, vec!["a", "b", "c"]);
    }

    #[test]
    fn plugins_concatenate_upstream_first() {
        let upstream = PluginSet { sharing: None, items: vec!["u1".into(), "u2".into()], configs: BTreeMap::new() };
        let route = PluginSet { sharing: None, items: vec!["u2".into(), "r1".into()], configs: BTreeMap::new() };
        assert_eq!(merge_plugins(Some(&upstream), Some(&route)), vec!["u1", "u2", "r1"]);
    }

    #[test]
    fn header_rules_layer_route_over_upstream() {
        use std::collections::BTreeMap;
        let mut upstream_set: BTreeMap<String, String> = BTreeMap::new();
        upstream_set.insert("x-up".into(), "1".into());
        let upstream = HeaderRules {
            request: Some(crate::domain::dto::HeaderRequestRules {
                set: upstream_set,
                ..Default::default()
            }),
            response: None,
        };
        let merged = merge_headers(Some(&upstream), None);
        assert_eq!(merged.request.unwrap().set.get("x-up").map(String::as_str), Some("1"));
    }

    #[test]
    fn cors_prefers_an_enforced_ancestor() {
        let ancestor = Cors { sharing: Some(SharingMode::Enforce), enabled: true, ..Cors::default() };
        let own = Cors { enabled: false, ..Cors::default() };
        assert_eq!(merge_cors(Some(&own), Some(&ancestor)), Some(ancestor));
    }

    #[test]
    fn apply_rate_limit_merges_a_late_layer() {
        let mut config = EffectiveConfig::default();
        apply_rate_limit(&mut config, &limit(50, Some(SharingMode::Enforce)), true);
        assert_eq!(config.rate_limit.as_ref().map(|r| r.capacity), Some(50));
        assert!(config.rate_limit_enforced);
    }
}
