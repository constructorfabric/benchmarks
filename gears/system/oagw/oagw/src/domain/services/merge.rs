//! Hierarchical configuration merge (`cpt-cf-oagw-fr-hierarchical-config`).
//!
//! Inputs are ordered **root first, calling tenant last**; the merge walks in
//! that direction so a descendant naturally overrides unless an ancestor
//! declared `sharing: enforce`.

use crate::domain::dto::{EffectiveConfig, ResolvedBinding, bindings_of};
use crate::domain::model::{
    AuthConfig, CorsConfig, HeadersConfig, RateLimitConfig, Route, SharingMode, Upstream,
};

/// One layer of the merge: a config block plus whether it belongs to the
/// calling tenant (own blocks are always visible, ancestors' only when shared).
pub struct Layer<'a> {
    pub upstream: &'a Upstream,
    pub is_own: bool,
}

/// Whether an ancestor block with this sharing mode is visible to descendants.
fn visible(sharing: SharingMode, is_own: bool) -> bool {
    is_own || matches!(sharing, SharingMode::Inherit | SharingMode::Enforce)
}

/// Auth: the closest visible block wins unless an ancestor enforced its own.
#[must_use]
pub fn merge_auth(layers: &[Layer<'_>]) -> Option<AuthConfig> {
    let mut chosen: Option<AuthConfig> = None;
    let mut locked = false;
    for layer in layers {
        let Some(auth) = layer.upstream.auth.as_ref() else {
            continue;
        };
        if !visible(auth.sharing, layer.is_own) {
            continue;
        }
        if locked {
            continue;
        }
        chosen = Some(auth.clone());
        locked = auth.sharing == SharingMode::Enforce && !layer.is_own;
    }
    chosen
}

/// Headers: later (closer) layers win per key; `remove` lists concatenate.
#[must_use]
pub fn merge_headers(layers: &[Layer<'_>]) -> HeadersConfig {
    let mut out = HeadersConfig::default();
    for layer in layers {
        let h = &layer.upstream.headers;
        if h.is_empty() {
            continue;
        }
        out.request.set.extend(h.request.set.clone());
        out.request.add.extend(h.request.add.clone());
        for name in &h.request.remove {
            if !out.request.remove.contains(name) {
                out.request.remove.push(name.clone());
            }
        }
        if h.request.passthrough != crate::domain::model::PassthroughMode::None {
            out.request.passthrough = h.request.passthrough;
        }
        for name in &h.request.passthrough_allowlist {
            if !out.request.passthrough_allowlist.contains(name) {
                out.request.passthrough_allowlist.push(name.clone());
            }
        }
        out.response.set.extend(h.response.set.clone());
        out.response.add.extend(h.response.add.clone());
        for name in &h.response.remove {
            if !out.response.remove.contains(name) {
                out.response.remove.push(name.clone());
            }
        }
    }
    out
}

/// Rate limits: strictest wins — `min(ancestor.enforced, ..., descendant)`.
///
/// The *shape* (scope, strategy, cost, algorithm) comes from the closest
/// visible block; only the numeric ceilings are minimised across the chain.
#[must_use]
pub fn merge_rate_limits(candidates: &[RateLimitConfig]) -> Option<RateLimitConfig> {
    let mut base = *candidates.last()?;
    let mut min_per_sec = base.refill_per_second();
    let mut min_capacity = base.capacity();
    for rl in candidates {
        let per_sec = rl.refill_per_second();
        if per_sec < min_per_sec {
            min_per_sec = per_sec;
        }
        if rl.capacity() < min_capacity {
            min_capacity = rl.capacity();
        }
    }
    // Re-express the winning rate in the base's window so the emitted
    // `X-RateLimit-Limit` stays in the operator's chosen unit.
    let window_secs = base.sustained.window.seconds();
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let rate = (min_per_sec * window_secs as f64).round().max(1.0) as u32;
    base.sustained.rate = rate;
    base.burst.capacity = Some(min_capacity);
    Some(base)
}

/// Collect the rate-limit blocks that apply: every visible upstream block plus
/// the matched route's, ordered root → route.
#[must_use]
pub fn rate_limit_candidates(
    layers: &[Layer<'_>],
    route: Option<&Route>,
) -> Vec<RateLimitConfig> {
    let mut out = Vec::new();
    for layer in layers {
        if let Some(rl) = layer.upstream.rate_limit.as_ref()
            && visible(rl.sharing, layer.is_own)
        {
            out.push(*rl);
        }
    }
    if let Some(rl) = route.and_then(|r| r.rate_limit.as_ref()) {
        out.push(*rl);
    }
    out
}

/// CORS: union origins when the ancestor shares with `inherit`; an ancestor
/// with `enforce` is used verbatim and descendants cannot add origins.
#[must_use]
pub fn merge_cors(layers: &[Layer<'_>], route: Option<&Route>) -> Option<CorsConfig> {
    let mut chosen: Option<CorsConfig> = None;
    let mut locked = false;

    for layer in layers {
        let Some(cors) = layer.upstream.cors.as_ref() else {
            continue;
        };
        if !visible(cors.sharing, layer.is_own) {
            continue;
        }
        if locked {
            continue;
        }
        match chosen.as_mut() {
            None => chosen = Some(cors.clone()),
            Some(acc) if acc.sharing == SharingMode::Inherit => {
                let mut merged = cors.clone();
                for origin in &acc.allowed_origins {
                    if !merged.allowed_origins.contains(origin) {
                        merged.allowed_origins.push(origin.clone());
                    }
                }
                *acc = merged;
            }
            Some(acc) => *acc = cors.clone(),
        }
        if cors.sharing == SharingMode::Enforce && !layer.is_own {
            locked = true;
        }
    }

    if let Some(route_cors) = route.and_then(|r| r.cors.as_ref())
        && !locked
    {
        match chosen.as_mut() {
            None => chosen = Some(route_cors.clone()),
            Some(acc) => {
                let mut merged = route_cors.clone();
                if acc.sharing == SharingMode::Inherit {
                    for origin in &acc.allowed_origins {
                        if !merged.allowed_origins.contains(origin) {
                            merged.allowed_origins.push(origin.clone());
                        }
                    }
                }
                *acc = merged;
            }
        }
    }
    chosen
}

/// Plugin chains concatenate: `ancestor.plugins + descendant.plugins`, then
/// the matched route's chain (upstream plugins always execute first).
#[must_use]
pub fn merge_plugins(layers: &[Layer<'_>], route: Option<&Route>) -> Vec<ResolvedBinding> {
    let mut out = Vec::new();
    for layer in layers {
        let cfg = &layer.upstream.plugins;
        if cfg.is_empty() || !visible(cfg.sharing, layer.is_own) {
            continue;
        }
        out.extend(bindings_of(cfg));
    }
    if let Some(route) = route {
        out.extend(bindings_of(&route.plugins));
    }
    out
}

/// Tags are add-only: `union(ancestor_tags, descendant_tags)`, no sharing mode.
#[must_use]
pub fn merge_tags(layers: &[Layer<'_>], route: Option<&Route>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for layer in layers {
        for tag in &layer.upstream.tags {
            if !out.contains(tag) {
                out.push(tag.clone());
            }
        }
    }
    if let Some(route) = route {
        for tag in &route.tags {
            if !out.contains(tag) {
                out.push(tag.clone());
            }
        }
    }
    out
}

/// Full effective configuration for a resolved (upstream chain, route) pair.
#[must_use]
pub fn effective_config(layers: &[Layer<'_>], route: Option<&Route>) -> EffectiveConfig {
    EffectiveConfig {
        auth: merge_auth(layers),
        headers: merge_headers(layers),
        rate_limit: merge_rate_limits(&rate_limit_candidates(layers, route)),
        cors: merge_cors(layers, route),
        plugins: merge_plugins(layers, route),
        tags: merge_tags(layers, route),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::model::{
        BurstConfig, PluginBinding, PluginsConfig, RateLimitAlgorithm, RateLimitScope,
        RateLimitStrategy, RateLimitWindow, ServerConfig, SustainedRate,
    };
    use std::collections::BTreeMap;
    use uuid::Uuid;

    fn upstream(alias: &str) -> Upstream {
        Upstream {
            id: Uuid::new_v4(),
            tenant_id: Uuid::new_v4(),
            alias: alias.to_owned(),
            protocol: crate::domain::gts_helpers::PROTOCOL_HTTP.to_owned(),
            enabled: true,
            server: ServerConfig { endpoints: vec![] },
            auth: None,
            headers: HeadersConfig::default(),
            rate_limit: None,
            cors: None,
            plugins: PluginsConfig::default(),
            tags: vec![],
            created_at: String::new(),
            updated_at: String::new(),
        }
    }

    fn rate(rate: u32, window: RateLimitWindow, sharing: SharingMode) -> RateLimitConfig {
        RateLimitConfig {
            sharing,
            algorithm: RateLimitAlgorithm::TokenBucket,
            sustained: SustainedRate { rate, window },
            burst: BurstConfig { capacity: None },
            budget: None,
            scope: RateLimitScope::Tenant,
            strategy: RateLimitStrategy::Reject,
            cost: 1,
            response_headers: true,
        }
    }

    fn auth(secret: &str, sharing: SharingMode) -> AuthConfig {
        let mut config = BTreeMap::new();
        config.insert("secret_ref".to_owned(), serde_json::json!(secret));
        AuthConfig {
            plugin_type: Some(crate::domain::gts_helpers::APIKEY_AUTH_PLUGIN_ID.to_owned()),
            sharing,
            config,
        }
    }

    #[test]
    fn descendant_auth_overrides_an_inherited_ancestor() {
        let mut root = upstream("api.openai.com");
        root.auth = Some(auth("cred://partner-openai-key", SharingMode::Inherit));
        let mut leaf = upstream("api.openai.com");
        leaf.auth = Some(auth("cred://my-own-openai-key", SharingMode::Private));

        let layers = vec![
            Layer {
                upstream: &root,
                is_own: false,
            },
            Layer {
                upstream: &leaf,
                is_own: true,
            },
        ];
        let merged = merge_auth(&layers).expect("auth");
        assert_eq!(
            merged.config.get("secret_ref").and_then(|v| v.as_str()),
            Some("cred://my-own-openai-key")
        );
    }

    #[test]
    fn an_enforcing_ancestor_cannot_be_overridden() {
        let mut root = upstream("api.openai.com");
        root.auth = Some(auth("cred://partner-openai-key", SharingMode::Enforce));
        let mut leaf = upstream("api.openai.com");
        leaf.auth = Some(auth("cred://my-own-openai-key", SharingMode::Private));

        let layers = vec![
            Layer {
                upstream: &root,
                is_own: false,
            },
            Layer {
                upstream: &leaf,
                is_own: true,
            },
        ];
        let merged = merge_auth(&layers).expect("auth");
        assert_eq!(
            merged.config.get("secret_ref").and_then(|v| v.as_str()),
            Some("cred://partner-openai-key")
        );
    }

    #[test]
    fn a_private_ancestor_is_invisible() {
        let mut root = upstream("api.openai.com");
        root.auth = Some(auth("cred://partner", SharingMode::Private));
        let leaf = upstream("api.openai.com");
        let layers = vec![
            Layer {
                upstream: &root,
                is_own: false,
            },
            Layer {
                upstream: &leaf,
                is_own: true,
            },
        ];
        assert!(merge_auth(&layers).is_none());
    }

    #[test]
    fn rate_limits_take_the_strictest_across_windows() {
        // Root enforces 10000/min, leaf asks for 100/min -> 100/min.
        let candidates = vec![
            rate(10_000, RateLimitWindow::Minute, SharingMode::Enforce),
            rate(100, RateLimitWindow::Minute, SharingMode::Private),
        ];
        let merged = merge_rate_limits(&candidates).expect("merged");
        assert_eq!(merged.sustained.rate, 100);
        assert_eq!(merged.sustained.window, RateLimitWindow::Minute);
    }

    #[test]
    fn rate_limits_normalise_windows_before_comparing() {
        // 1/second (=60/min) is stricter than 100/min; the base window is the
        // last (closest) block, i.e. minute.
        let candidates = vec![
            rate(1, RateLimitWindow::Second, SharingMode::Enforce),
            rate(100, RateLimitWindow::Minute, SharingMode::Private),
        ];
        let merged = merge_rate_limits(&candidates).expect("merged");
        assert_eq!(merged.sustained.window, RateLimitWindow::Minute);
        assert_eq!(merged.sustained.rate, 60);
    }

    #[test]
    fn an_ancestor_private_rate_limit_does_not_apply() {
        let mut root = upstream("a");
        root.rate_limit = Some(rate(1, RateLimitWindow::Second, SharingMode::Private));
        let mut leaf = upstream("a");
        leaf.rate_limit = Some(rate(500, RateLimitWindow::Second, SharingMode::Private));
        let layers = vec![
            Layer {
                upstream: &root,
                is_own: false,
            },
            Layer {
                upstream: &leaf,
                is_own: true,
            },
        ];
        let merged =
            merge_rate_limits(&rate_limit_candidates(&layers, None)).expect("leaf-only limit");
        assert_eq!(merged.sustained.rate, 500);
    }

    #[test]
    fn plugin_chains_concatenate_ancestor_first() {
        let mut root = upstream("a");
        root.plugins = PluginsConfig {
            sharing: SharingMode::Inherit,
            items: vec![PluginBinding::new("U1".to_owned(), BTreeMap::new())],
        };
        let mut leaf = upstream("a");
        leaf.plugins = PluginsConfig {
            sharing: SharingMode::Private,
            items: vec![PluginBinding::new("U2".to_owned(), BTreeMap::new())],
        };
        let layers = vec![
            Layer {
                upstream: &root,
                is_own: false,
            },
            Layer {
                upstream: &leaf,
                is_own: true,
            },
        ];
        let chain = merge_plugins(&layers, None);
        let refs: Vec<&str> = chain.iter().map(|b| b.plugin_ref.as_str()).collect();
        assert_eq!(refs, vec!["U1", "U2"]);
    }

    #[test]
    fn tags_are_add_only_union() {
        let mut root = upstream("a");
        root.tags = vec!["openai".to_owned()];
        let mut leaf = upstream("a");
        leaf.tags = vec!["llm".to_owned(), "openai".to_owned()];
        let layers = vec![
            Layer {
                upstream: &root,
                is_own: false,
            },
            Layer {
                upstream: &leaf,
                is_own: true,
            },
        ];
        assert_eq!(merge_tags(&layers, None), vec!["openai", "llm"]);
    }

    #[test]
    fn cors_origins_union_under_inherit() {
        let mut root = upstream("a");
        root.cors = Some(CorsConfig {
            sharing: SharingMode::Inherit,
            enabled: true,
            allowed_origins: vec!["https://app.example.com".to_owned()],
            allowed_methods: vec!["GET".to_owned()],
            expose_headers: vec![],
            allow_credentials: false,
        });
        let mut leaf = upstream("a");
        leaf.cors = Some(CorsConfig {
            sharing: SharingMode::Private,
            enabled: true,
            allowed_origins: vec!["https://admin.example.com".to_owned()],
            allowed_methods: vec!["GET".to_owned()],
            expose_headers: vec![],
            allow_credentials: false,
        });
        let layers = vec![
            Layer {
                upstream: &root,
                is_own: false,
            },
            Layer {
                upstream: &leaf,
                is_own: true,
            },
        ];
        let merged = merge_cors(&layers, None).expect("cors");
        assert!(merged.allows_origin("https://app.example.com"));
        assert!(merged.allows_origin("https://admin.example.com"));
    }
}
