//! Tenant-hierarchy walk and effective-config merge.
//!
//! Alias resolution walks descendant → root and the closest match wins; an
//! ancestor's *enforced* limits still apply across shadowing. Merge rules:
//! `min()` for rate limits, union for tags and CORS origins, concatenation for
//! plugin chains.

use uuid::Uuid;

use crate::domain::dto::{
    CorsConfig, HeadersConfig, PluginsConfig, RateLimitConfig, Route, Sharing, Upstream,
};

/// The upstream a request resolved to, with everything the chain contributed.
#[derive(Debug, Clone)]
pub struct ResolvedUpstream {
    /// The upstream that matched.
    pub upstream: Upstream,
    /// The tenant that owns it.
    pub tenant_id: Uuid,
    /// Every tenant id in the walk, closest first.
    pub chain: Vec<Uuid>,
}

/// Ancestor's configuration, in `descendant → root` order.
#[derive(Debug, Clone, Default)]
pub struct ChainConfig {
    /// Upstreams seen during the walk, closest first.
    pub upstreams: Vec<Upstream>,
    /// Routes seen for the resolved upstream, closest first.
    pub routes: Vec<Route>,
}

impl ChainConfig {
    /// The effective upstream: the closest match in the walk.
    #[must_use]
    pub fn effective_upstream(&self) -> Option<&Upstream> {
        self.upstreams.first()
    }

    /// Whether any ancestor disabled the effective upstream.
    #[must_use]
    pub fn is_disabled(&self) -> bool {
        self.upstreams.iter().any(|upstream| !upstream.enabled)
    }
}

/// Merges two header-rule blocks, `override` winning on conflicts.
#[must_use]
pub fn merge_headers(base: Option<&HeadersConfig>, over: Option<&HeadersConfig>) -> HeadersConfig {
    match (base, over) {
        (None, None) => HeadersConfig::default(),
        (Some(only), None) | (None, Some(only)) => only.clone(),
        (Some(base), Some(over)) => {
            let mut merged = HeadersConfig {
                request: None,
                response: None,
            };
            merged.request = match (&base.request, &over.request) {
                (None, None) => None,
                (Some(only), None) | (None, Some(only)) => Some(only.clone()),
                (Some(base), Some(over)) => Some(crate::domain::dto::RequestHeaderRules {
                    set: merge_maps(&base.set, &over.set),
                    add: merge_maps(&base.add, &over.add),
                    remove: {
                        let mut items = base.remove.clone();
                        for name in &over.remove {
                            if !items.contains(name) {
                                items.push(name.clone());
                            }
                        }
                        items
                    },
                    passthrough: over.passthrough,
                    passthrough_allowlist: {
                        let mut items = base.passthrough_allowlist.clone();
                        for name in &over.passthrough_allowlist {
                            if !items.contains(name) {
                                items.push(name.clone());
                            }
                        }
                        items
                    },
                }),
            };
            merged.response = match (&base.response, &over.response) {
                (None, None) => None,
                (Some(only), None) | (None, Some(only)) => Some(only.clone()),
                (Some(base), Some(over)) => Some(crate::domain::dto::ResponseHeaderRules {
                    set: merge_maps(&base.set, &over.set),
                    add: merge_maps(&base.add, &over.add),
                    remove: {
                        let mut items = base.remove.clone();
                        for name in &over.remove {
                            if !items.contains(name) {
                                items.push(name.clone());
                            }
                        }
                        items
                    },
                }),
            };
            merged
        }
    }
}

fn merge_maps(
    base: &std::collections::BTreeMap<String, String>,
    over: &std::collections::BTreeMap<String, String>,
) -> std::collections::BTreeMap<String, String> {
    let mut merged = base.clone();
    for (key, value) in over {
        merged.insert(key.clone(), value.clone());
    }
    merged
}

/// Unions two tag sets, preserving order and dropping duplicates.
#[must_use]
pub fn merge_tags(base: &[String], over: &[String]) -> Vec<String> {
    let mut merged = base.to_vec();
    for tag in over {
        if !merged.contains(tag) {
            merged.push(tag.clone());
        }
    }
    merged
}

/// Concatenates two plugin chains, ancestor first.
#[must_use]
pub fn merge_plugins(base: Option<&PluginsConfig>, over: Option<&PluginsConfig>) -> PluginsConfig {
    match (base, over) {
        (None, None) => PluginsConfig::default(),
        (Some(only), None) | (None, Some(only)) => only.clone(),
        (Some(base), Some(over)) => PluginsConfig {
            sharing: over.sharing,
            items: {
                let mut items = base.items.clone();
                for item in &over.items {
                    if !items.contains(item) {
                        items.push(item.clone());
                    }
                }
                items
            },
            config: merge_plugin_config(&base.config, &over.config),
        },
    }
}

fn merge_plugin_config(
    base: &std::collections::BTreeMap<String, serde_json::Value>,
    over: &std::collections::BTreeMap<String, serde_json::Value>,
) -> std::collections::BTreeMap<String, serde_json::Value> {
    let mut merged = base.clone();
    for (key, value) in over {
        merged.insert(key.clone(), value.clone());
    }
    merged
}

/// Merges two rate limits: the tighter budget wins, and `enforce` blocks the
/// descendant from loosening it.
#[must_use]
pub fn merge_rate_limit(
    base: Option<&RateLimitConfig>,
    over: Option<&RateLimitConfig>,
) -> Option<RateLimitConfig> {
    match (base, over) {
        (None, None) => None,
        (Some(only), None) | (None, Some(only)) => Some(only.clone()),
        (Some(base), Some(over)) => {
            // The budget itself is order-independent: the tighter of the two
            // levels wins, so a descendant can tighten but never loosen. Only
            // who the merged policy answers to is directional — `over` is the
            // more specific level, and an ancestor's `enforce` survives it.
            let capacity = base.capacity().min(over.capacity());
            let sustained = if base.refill_per_second() <= over.refill_per_second() {
                base.sustained
            } else {
                over.sustained
            };
            Some(RateLimitConfig {
                sharing: if base.sharing == Sharing::Enforce {
                    Sharing::Enforce
                } else {
                    over.sharing
                },
                algorithm: over.algorithm,
                sustained,
                burst: Some(crate::domain::dto::Burst { capacity }),
                scope: over.scope,
                strategy: over.strategy,
                cost: over.cost,
            })
        }
    }
}

/// Merges two CORS blocks; the descendant wins unless the ancestor enforces.
#[must_use]
pub fn merge_cors(base: Option<&CorsConfig>, over: Option<&CorsConfig>) -> Option<CorsConfig> {
    match (base, over) {
        (None, None) => None,
        (Some(only), None) | (None, Some(only)) => Some(only.clone()),
        (Some(base), Some(over)) => {
            if base.sharing == Sharing::Enforce {
                return Some(base.clone());
            }
            Some(CorsConfig {
                sharing: over.sharing,
                enabled: over.enabled || base.enabled,
                allowed_origins: {
                    // A wildcard anywhere collapses the whole list: it grants
                    // everything the narrower entries already granted.
                    if base.allowed_origins.iter().chain(&over.allowed_origins)
                        .any(|origin| origin == "*")
                    {
                        vec!["*".to_owned()]
                    } else {
                        merge_tags(&base.allowed_origins, &over.allowed_origins)
                    }
                },
                allowed_methods: merge_tags(&base.allowed_methods, &over.allowed_methods),
                expose_headers: merge_tags(&base.expose_headers, &over.expose_headers),
                allow_credentials: over.allow_credentials || base.allow_credentials,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::dto::{Burst, SustainedRate, RateWindow};

    fn rate(rate: u64, _seconds: u64) -> RateLimitConfig {
        RateLimitConfig {
            sharing: Sharing::Inherit,
            algorithm: crate::domain::dto::RateAlgorithm::TokenBucket,
            sustained: SustainedRate {
                rate,
                window: RateWindow::Second,
            },
            burst: Some(Burst { capacity: rate }),
            scope: crate::domain::dto::RateScope::Tenant,
            strategy: crate::domain::dto::RateStrategy::Reject,
            cost: 1,
        }
    }

    #[test]
    fn rate_limit_merge_takes_the_minimum() {
        let merged = merge_rate_limit(Some(&rate(100, 1)), Some(&rate(50, 1))).expect("merged");
        assert_eq!(merged.capacity(), 50);
    }

    #[test]
    fn rate_limit_enforce_blocks_loosening() {
        let mut ancestor = rate(10, 1);
        ancestor.sharing = Sharing::Enforce;
        let merged = merge_rate_limit(Some(&ancestor), Some(&rate(1000, 1))).expect("merged");
        assert_eq!(merged.capacity(), 10);
    }

    #[test]
    fn tags_are_unioned() {
        let merged = merge_tags(&["a".into(), "b".into()], &["b".into(), "c".into()]);
        assert_eq!(merged, vec!["a", "b", "c"]);
    }

    #[test]
    fn plugins_are_concatenated_without_duplicates() {
        let base = PluginsConfig {
            sharing: Sharing::Private,
            items: vec!["p1".into()],
            config: std::collections::BTreeMap::new(),
        };
        let over = PluginsConfig {
            sharing: Sharing::Private,
            items: vec!["p1".into(), "p2".into()],
            config: std::collections::BTreeMap::new(),
        };
        let merged = merge_plugins(Some(&base), Some(&over));
        assert_eq!(merged.items, vec!["p1", "p2"]);
    }

    #[test]
    fn upstream_plugins_run_before_route_plugins() {
        let upstream = PluginsConfig {
            sharing: Sharing::Private,
            items: vec!["u1".into(), "u2".into()],
            config: std::collections::BTreeMap::new(),
        };
        let route = PluginsConfig {
            sharing: Sharing::Private,
            items: vec!["r1".into(), "r2".into()],
            config: std::collections::BTreeMap::new(),
        };
        let merged = merge_plugins(Some(&upstream), Some(&route));
        assert_eq!(merged.items, vec!["u1", "u2", "r1", "r2"]);
    }

    #[test]
    fn cors_origins_are_unioned() {
        let base = CorsConfig {
            sharing: Sharing::Inherit,
            enabled: true,
            allowed_origins: vec!["https://a.example.com".into()],
            allowed_methods: vec!["GET".into()],
            expose_headers: vec![],
            allow_credentials: false,
        };
        let over = CorsConfig {
            sharing: Sharing::Inherit,
            enabled: true,
            allowed_origins: vec!["https://b.example.com".into()],
            allowed_methods: vec!["POST".into()],
            expose_headers: vec![],
            allow_credentials: false,
        };
        let merged = merge_cors(Some(&base), Some(&over)).expect("merged");
        assert_eq!(merged.allowed_origins.len(), 2);
        assert_eq!(merged.allowed_methods.len(), 2);
    }

    #[test]
    fn wildcard_origin_dominates() {
        let base = CorsConfig {
            sharing: Sharing::Inherit,
            enabled: true,
            allowed_origins: vec!["https://a.example.com".into()],
            allowed_methods: vec![],
            expose_headers: vec![],
            allow_credentials: false,
        };
        let over = CorsConfig {
            sharing: Sharing::Inherit,
            enabled: true,
            allowed_origins: vec!["*".into()],
            allowed_methods: vec![],
            expose_headers: vec![],
            allow_credentials: false,
        };
        let merged = merge_cors(Some(&base), Some(&over)).expect("merged");
        assert_eq!(merged.allowed_origins, vec!["*"]);
    }
}
