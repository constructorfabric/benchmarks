//! Hierarchical configuration merge across the tenant chain.
//!
//! Implements the per-field sharing-mode merge semantics (DoD
//! `cpt-cf-oagw-dod-domain-model-repositories-merge`, algorithm
//! `cpt-cf-oagw-algo-domain-model-repositories-merge`, flow
//! `cpt-cf-oagw-flow-domain-model-repositories-effective`):
//!
//! - **auth**: descendant overrides under `inherit`; ancestor value forced
//!   under `enforce` (descendant contribution ignored);
//! - **rate limits**: `min(ancestor, descendant)` across the hierarchy
//!   (descendants can only be stricter);
//! - **plugins**: ancestor bindings concatenated before descendant bindings
//!   (enforced plugin chains cannot be removed; positions re-derived
//!   contiguous from 0);
//! - **CORS**: origin/method union under `inherit`; ancestor set forced under
//!   `enforce`;
//! - **tags**: add-only union (descendants may add, never remove).
//!
//! `private` hides a configuration from descendants — it only applies to the
//! tenant that owns it.

use crate::domain::entity::config::{
    CorsConfig, PluginBinding, PluginsConfig, RateLimitConfig, SharingMode,
};
use crate::domain::entity::upstream::{AuthConfig, Upstream};

/// Effective (merged) upstream configuration for a leaf tenant.
///
/// Produced by walking the tenant chain ancestor → leaf and applying the
/// per-field sharing-mode rules.  Consumed by the Data Plane (effective-config
/// application) and later cached per the data-plane caching ADR.
// NOTE: no `Eq` — `AuthConfig`/`PluginsConfig` carry `serde_json::Value`
// (not `Eq`), so structural equality is `PartialEq`-only by construction.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct UpstreamConfig {
    pub auth: Option<AuthConfig>,
    pub rate_limit: Option<RateLimitConfig>,
    pub cors: Option<CorsConfig>,
    pub plugins: PluginsConfig,
    /// Add-only union of tags across the hierarchy.
    pub tags: Vec<String>,
}

impl UpstreamConfig {
    /// Merges the configuration of `ancestors` (root first) with the leaf
    /// tenant's own upstream definition.
    #[must_use]
    pub fn merge_hierarchy(ancestors: &[&Upstream], leaf: &Upstream) -> Self {
        let mut chain: Vec<&Upstream> = ancestors.to_vec();
        chain.push(leaf);
        Self::merge_chain(&chain)
    }

    /// Merges a full tenant chain ordered **root → leaf** (ancestors first,
    /// target tenant last).
    #[must_use]
    pub fn merge_chain(chain: &[&Upstream]) -> Self {
        let count = chain.len();
        let is_leaf = |idx: usize| idx + 1 == count;

        // --- auth ---
        let mut auth: Option<AuthConfig> = None;
        let mut auth_forced = false;
        for (idx, up) in chain.iter().enumerate() {
            let a = &up.auth;
            if a.plugin_type.is_some() || !a.config.is_null() {
                match a.sharing {
                    SharingMode::Enforce => {
                        auth = Some(a.clone());
                        auth_forced = true;
                    }
                    SharingMode::Inherit => {
                        if !auth_forced {
                            // Descendant overrides under inherit.
                            auth = Some(a.clone());
                        }
                    }
                    SharingMode::Private => {
                        // `private` hides a binding from descendants, but an
                        // ancestor-enforced auth can never be overridden — even
                        // by the leaf's own private binding (`inst-dm-eff-enforce`).
                        if is_leaf(idx) && !auth_forced {
                            auth = Some(a.clone());
                        }
                    }
                }
            }
        }

        // --- rate limits (min across the hierarchy) ---
        let mut rate_limit: Option<RateLimitConfig> = None;
        for (idx, up) in chain.iter().enumerate() {
            let Some(rl) = &up.rate_limit else { continue };
            if rl.sharing == SharingMode::Private && !is_leaf(idx) {
                continue;
            }
            rate_limit = Some(match rate_limit {
                None => rl.clone(),
                Some(acc) => merge_rate_limits(acc, rl),
            });
        }

        // --- CORS (union under inherit, ancestor set forced under enforce) ---
        let mut cors: Option<CorsConfig> = None;
        let mut cors_forced = false;
        for (idx, up) in chain.iter().enumerate() {
            let Some(c) = &up.cors else { continue };
            if c.sharing == SharingMode::Private && !is_leaf(idx) {
                continue;
            }
            if c.sharing == SharingMode::Enforce && !is_leaf(idx) {
                // An ancestor-enforced set replaces whatever was accumulated
                // and cannot be extended by descendants.
                cors = Some(c.clone());
                cors_forced = true;
            } else if !cors_forced {
                // Union under inherit (leaf's own set is always honored).
                cors = Some(match cors.take() {
                    None => c.clone(),
                    Some(acc) => merge_cors_union(acc, c),
                });
            }
        }

        // --- plugins (ancestor first, enforced chains always included) ---
        let mut items: Vec<PluginBinding> = Vec::new();
        let mut effective_sharing = SharingMode::Private;
        for (idx, up) in chain.iter().enumerate() {
            let cfg = &up.plugins;
            if cfg.sharing == SharingMode::Private && !is_leaf(idx) {
                continue;
            }
            if is_leaf(idx) {
                // The leaf's own bindings are always honored.
                for b in &cfg.items {
                    items.push(reposition(b, items.len()));
                }
                effective_sharing = cfg.sharing;
            } else if cfg.sharing == SharingMode::Enforce {
                // Enforced ancestor plugins cannot be removed by descendants.
                for b in &cfg.items {
                    items.push(reposition(b, items.len()));
                }
                effective_sharing = SharingMode::Enforce;
            } else if cfg.sharing == SharingMode::Inherit {
                for b in &cfg.items {
                    items.push(reposition(b, items.len()));
                }
            }
        }
        let plugins = PluginsConfig {
            sharing: effective_sharing,
            items,
        };

        // --- tags (add-only union) ---
        // Tags are always additive across the hierarchy; `private` does not
        // gate tag aggregation per the add-only union rule.
        let mut tags: Vec<String> = Vec::new();
        for up in chain.iter() {
            for tag in &up.tags {
                if !tags.contains(tag) {
                    tags.push(tag.clone());
                }
            }
        }

        Self {
            auth,
            rate_limit,
            cors,
            plugins,
            tags,
        }
    }
}

/// Merges two rate limits keeping the stricter (minimal) sustained rate.
/// The resulting configuration preserves the algorithm/scope/strategy/cost of
/// the stricter side; `sustained.rate` and `burst.capacity` are minimized.
#[must_use]
fn merge_rate_limits(acc: RateLimitConfig, next: &RateLimitConfig) -> RateLimitConfig {
    let acc_rate = acc.sustained.rate;
    let next_rate = next.sustained.rate;
    if next_rate < acc_rate {
        // `next` is stricter: keep its algorithm/scope/strategy/cost.
        RateLimitConfig {
            sharing: acc.sharing,
            sustained: next.sustained,
            burst: min_burst(acc.burst, next.burst),
            ..next.clone()
        }
    } else {
        RateLimitConfig {
            sustained: acc.sustained,
            burst: min_burst(acc.burst, next.burst),
            ..acc
        }
    }
}

fn min_burst(
    acc: Option<crate::domain::entity::config::BurstConfig>,
    next: Option<crate::domain::entity::config::BurstConfig>,
) -> Option<crate::domain::entity::config::BurstConfig> {
    match (acc, next) {
        (Some(a), Some(b)) => Some(crate::domain::entity::config::BurstConfig {
            capacity: match (a.capacity, b.capacity) {
                (Some(x), Some(y)) => Some(x.min(y)),
                (None, y) => y,
                (x, None) => x,
            },
        }),
        (a, None) => a,
        (None, b) => b,
    }
}

/// Unions two CORS configs under `inherit`: origins/methods/exposed headers
/// are combined; the result is enabled when either side is.
///
/// `pub(crate)` so the Data Plane can layer a matched route's CORS onto the
/// tenant-effective base with the same union semantics (route overlay,
/// `inst-dp-cfg-route`).
#[must_use]
pub(crate) fn merge_cors_union(acc: CorsConfig, next: &CorsConfig) -> CorsConfig {
    let mut allowed_origins = acc.allowed_origins.clone();
    for o in &next.allowed_origins {
        if !allowed_origins.contains(o) {
            allowed_origins.push(o.clone());
        }
    }
    let mut allowed_methods = acc.allowed_methods.clone();
    for m in &next.allowed_methods {
        if !allowed_methods.contains(m) {
            allowed_methods.push(m.clone());
        }
    }
    let mut expose_headers = acc.expose_headers.clone();
    for h in &next.expose_headers {
        if !expose_headers.contains(h) {
            expose_headers.push(h.clone());
        }
    }
    CorsConfig {
        sharing: acc.sharing,
        enabled: acc.enabled || next.enabled,
        allowed_origins,
        allowed_methods,
        expose_headers,
        allow_credentials: acc.allow_credentials || next.allow_credentials,
    }
}

/// Re-positions a binding and marks it owned by the current chain node.
fn reposition(binding: &PluginBinding, position: usize) -> PluginBinding {
    let mut b = binding.clone();
    b.position = u32::try_from(position).unwrap_or(u32::MAX);
    b
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::entity::config::{BurstConfig, CorsConfig, RateLimitConfig, SustainedRate};
    use serde_json::json;

    fn upstream(
        auth: AuthConfig,
        rate: Option<RateLimitConfig>,
        cors: Option<CorsConfig>,
        plugins: Vec<&str>,
        tags: Vec<&str>,
    ) -> Upstream {
        let mut u = Upstream::new(uuid::Uuid::from_u128(1), "alias", Default::default());
        u.auth = auth;
        u.rate_limit = rate;
        u.cors = cors;
        u.plugins = PluginsConfig {
            // Plugin chains default to `inherit` so ancestor bindings are
            // visible to descendants (concatenated ancestor-first).
            sharing: SharingMode::Inherit,
            items: plugins
                .iter()
                .enumerate()
                .map(|(i, p)| PluginBinding {
                    position: i as u32,
                    plugin_ref: p.to_string(),
                    plugin_uuid: None,
                    config: json!({}),
                })
                .collect(),
        };
        u.tags = tags.iter().map(|t| t.to_string()).collect();
        u
    }

    fn auth(plugin_type: &str, sharing: SharingMode) -> AuthConfig {
        AuthConfig {
            plugin_type: Some(plugin_type.to_owned()),
            sharing,
            config: json!({ "client_id": "x" }),
        }
    }

    fn rate(rate: u64, sharing: SharingMode) -> RateLimitConfig {
        RateLimitConfig {
            sharing,
            sustained: SustainedRate {
                rate,
                window: Default::default(),
            },
            ..RateLimitConfig::default()
        }
    }

    fn cors_allowed(origins: &[&str], sharing: SharingMode) -> CorsConfig {
        CorsConfig {
            sharing,
            allowed_origins: origins.iter().map(|s| s.to_string()).collect(),
            ..CorsConfig::default()
        }
    }

    #[test]
    fn descendant_overrides_under_inherit() {
        let ancestor = upstream(
            auth("apikey", SharingMode::Inherit),
            None,
            None,
            vec![],
            vec![],
        );
        let leaf = upstream(
            auth("oauth2", SharingMode::Inherit),
            None,
            None,
            vec![],
            vec![],
        );
        let eff = UpstreamConfig::merge_hierarchy(&[&ancestor], &leaf);
        let a = eff.auth.expect("auth present");
        assert_eq!(a.plugin_type.as_deref(), Some("oauth2"));
    }

    #[test]
    fn enforce_forces_ancestor_auth_over_descendant() {
        let ancestor = upstream(
            auth("apikey", SharingMode::Enforce),
            None,
            None,
            vec![],
            vec![],
        );
        let leaf = upstream(
            auth("oauth2", SharingMode::Inherit),
            None,
            None,
            vec![],
            vec![],
        );
        let eff = UpstreamConfig::merge_hierarchy(&[&ancestor], &leaf);
        let a = eff.auth.expect("auth present");
        assert_eq!(a.plugin_type.as_deref(), Some("apikey"));
    }

    #[test]
    fn enforce_forces_ancestor_auth_over_descendant_private_leaf() {
        // The leaf's own `private` binding must not override an ancestor-
        // enforced auth (`inst-dm-eff-enforce`).
        let ancestor = upstream(
            auth("apikey", SharingMode::Enforce),
            None,
            None,
            vec![],
            vec![],
        );
        let leaf = upstream(
            auth("oauth2", SharingMode::Private),
            None,
            None,
            vec![],
            vec![],
        );
        let eff = UpstreamConfig::merge_hierarchy(&[&ancestor], &leaf);
        let a = eff.auth.expect("auth present");
        assert_eq!(a.plugin_type.as_deref(), Some("apikey"));
    }

    #[test]
    fn rate_limits_use_min() {
        let ancestor = upstream(
            Default::default(),
            Some(rate(100, SharingMode::Inherit)),
            None,
            vec![],
            vec![],
        );
        let leaf = upstream(
            Default::default(),
            Some(rate(10, SharingMode::Inherit)),
            None,
            vec![],
            vec![],
        );
        let eff = UpstreamConfig::merge_hierarchy(&[&ancestor], &leaf);
        assert_eq!(eff.rate_limit.unwrap().sustained.rate, 10);

        // Descendant can only be stricter: a larger descendant rate is capped
        // by the ancestor's smaller one.
        let ancestor = upstream(
            Default::default(),
            Some(rate(10, SharingMode::Inherit)),
            None,
            vec![],
            vec![],
        );
        let leaf = upstream(
            Default::default(),
            Some(rate(100, SharingMode::Inherit)),
            None,
            vec![],
            vec![],
        );
        let eff = UpstreamConfig::merge_hierarchy(&[&ancestor], &leaf);
        assert_eq!(eff.rate_limit.unwrap().sustained.rate, 10);
    }

    #[test]
    fn plugins_concat_ancestor_first() {
        let ancestor = upstream(Default::default(), None, None, vec!["u1", "u2"], vec![]);
        let leaf = upstream(Default::default(), None, None, vec!["r1"], vec![]);
        let eff = UpstreamConfig::merge_hierarchy(&[&ancestor], &leaf);
        let refs: Vec<&str> = eff
            .plugins
            .items
            .iter()
            .map(|b| b.plugin_ref.as_str())
            .collect();
        assert_eq!(refs, vec!["u1", "u2", "r1"]);
        // Contiguous positions from 0.
        for (i, b) in eff.plugins.items.iter().enumerate() {
            assert_eq!(b.position as usize, i);
        }
    }

    #[test]
    fn private_plugin_chain_is_hidden_from_descendants() {
        let mut ancestor = upstream(Default::default(), None, None, vec!["u1"], vec![]);
        ancestor.plugins.sharing = SharingMode::Private;
        let leaf = upstream(Default::default(), None, None, vec!["r1"], vec![]);
        let eff = UpstreamConfig::merge_hierarchy(&[&ancestor], &leaf);
        let refs: Vec<&str> = eff
            .plugins
            .items
            .iter()
            .map(|b| b.plugin_ref.as_str())
            .collect();
        assert_eq!(refs, vec!["r1"]);
    }

    #[test]
    fn cors_union_under_inherit() {
        let ancestor = upstream(
            Default::default(),
            None,
            Some(cors_allowed(&["https://a.com"], SharingMode::Inherit)),
            vec![],
            vec![],
        );
        let leaf = upstream(
            Default::default(),
            None,
            Some(cors_allowed(&["https://b.com"], SharingMode::Inherit)),
            vec![],
            vec![],
        );
        let eff = UpstreamConfig::merge_hierarchy(&[&ancestor], &leaf);
        let c = eff.cors.expect("cors present");
        assert!(c.allowed_origins.contains(&"https://a.com".to_string()));
        assert!(c.allowed_origins.contains(&"https://b.com".to_string()));
    }

    #[test]
    fn cors_enforce_forces_ancestor_set() {
        let ancestor = upstream(
            Default::default(),
            None,
            Some(cors_allowed(&["https://a.com"], SharingMode::Enforce)),
            vec![],
            vec![],
        );
        let leaf = upstream(
            Default::default(),
            None,
            Some(cors_allowed(&["https://b.com"], SharingMode::Inherit)),
            vec![],
            vec![],
        );
        let eff = UpstreamConfig::merge_hierarchy(&[&ancestor], &leaf);
        let c = eff.cors.expect("cors present");
        assert_eq!(c.allowed_origins, vec!["https://a.com".to_string()]);
    }

    #[test]
    fn tags_are_additively_merged() {
        let ancestor = upstream(
            Default::default(),
            None,
            None,
            vec![],
            vec!["openai", "llm"],
        );
        let leaf = upstream(Default::default(), None, None, vec![], vec!["llm", "chat"]);
        let eff = UpstreamConfig::merge_hierarchy(&[&ancestor], &leaf);
        assert_eq!(eff.tags, vec!["openai", "llm", "chat"]);
        // A descendant cannot remove an inherited tag.
        assert!(eff.tags.contains(&"openai".to_string()));
    }

    #[test]
    fn empty_chain_produces_defaults() {
        let leaf = upstream(Default::default(), None, None, vec![], vec![]);
        let eff = UpstreamConfig::merge_hierarchy(&[], &leaf);
        assert_eq!(eff.auth, None);
        assert_eq!(eff.rate_limit, None);
        assert_eq!(eff.cors, None);
        assert!(eff.plugins.items.is_empty());
        assert!(eff.tags.is_empty());
    }

    #[test]
    fn min_burst_capacity() {
        let a = BurstConfig { capacity: Some(50) };
        let b = BurstConfig { capacity: Some(10) };
        let merged = min_burst(Some(a), Some(b));
        assert_eq!(merged.unwrap().capacity, Some(10));
    }

    #[test]
    fn leaf_headers_not_part_of_merge_surface() {
        // Headers are per-tenant server-side configuration, not merged.
        let mut leaf = upstream(Default::default(), None, None, vec![], vec![]);
        leaf.headers
            .request
            .set
            .insert("X-Foo".to_owned(), "bar".to_owned());
        let eff = UpstreamConfig::merge_hierarchy(&[], &leaf);
        assert!(eff.auth.is_none());
    }
}
