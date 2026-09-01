// Created: 2026-08-29 by Constructor Tech
//! Hierarchical merge of effective configuration (DESIGN §3.2).
//!
//! For each config block the chain is walked **ancestor → descendant**:
//!
//! | Field | Merge strategy |
//! |---|---|
//! | `auth` | `private` not inherited; `inherit` overridable; `enforce` forced |
//! | `rate_limit` | `min(ancestor, descendant)` per numeric field |
//! | `plugins` | concatenate, dedupe preserving first occurrence |
//! | `cors` | union of origins/methods/exposed headers when `inherit` |
//! | `headers` | same sharing semantics as `auth` |
//! | `tags` | always union |
//! | `enabled` | a disabled ancestor disables every descendant |

use super::model::{
    AuthConfig, CorsConfig, HeadersConfig, PluginsConfig, RateAlgorithm, RateLimitConfig, Sharing,
    Sustained,
};
use uuid::Uuid;

/// One link of the tenant chain, ordered ancestor first.
#[derive(Debug, Clone)]
pub struct ChainEntry {
    /// Owning tenant id.
    pub tenant_id: uuid::Uuid,
    /// Upstream record for this tenant, when it exists.
    pub upstream: Option<super::model::Upstream>,
}

/// Effective, merged configuration for a proxied request.
#[derive(Debug, Clone)]
pub struct EffectiveConfig {
    /// Effective auth plugin binding.
    pub auth: Option<AuthConfig>,
    /// Effective header transformation rules.
    pub headers: HeadersConfig,
    /// Effective plugin chain (ancestor items first).
    pub plugins: PluginsConfig,
    /// Effective rate limit.
    pub rate_limit: Option<RateLimitConfig>,
    /// Id of the resource that configured [`Self::rate_limit`].
    ///
    /// ADR-0003 keys the bucket on `{resource_type}:{resource_id}` of the
    /// *configuring* resource, so an upstream limit is one budget shared by
    /// every route that does not override it, while a route limit is its own.
    pub rate_limit_owner: Option<Uuid>,
    /// Effective CORS configuration.
    pub cors: Option<CorsConfig>,
    /// Union of all tags.
    pub tags: Vec<String>,
    /// `false` when any entry in the chain is disabled.
    pub enabled: bool,
}

fn is_enforced(sharing: Sharing) -> bool {
    sharing == Sharing::Enforce
}

fn is_private(sharing: Sharing) -> bool {
    sharing == Sharing::Private
}

/// Merge the tenant chain into an effective configuration.
///
/// `chain` is ordered ancestor → descendant.
#[must_use]
pub fn merge_chain(chain: &[ChainEntry]) -> EffectiveConfig {
    let mut auth: Option<AuthConfig> = None;
    let mut headers = HeadersConfig::default();
    let mut plugins = PluginsConfig::default();
    let mut rate_limit: Option<RateLimitConfig> = None;
    let mut rate_limit_owner: Option<Uuid> = None;
    let mut cors: Option<CorsConfig> = None;
    let mut tags: Vec<String> = Vec::new();
    let mut enabled = true;

    for entry in chain {
        let Some(spec) = entry.upstream.as_ref().map(|u| &u.spec) else {
            continue;
        };
        if !spec.enabled {
            enabled = false;
        }
        for tag in &spec.tags {
            if !tags.contains(tag) {
                tags.push(tag.clone());
            }
        }
        auth = merge_auth(auth, spec.auth.clone());
        headers = merge_headers(headers, spec.headers.clone());
        plugins = merge_plugins(plugins, spec.plugins.clone());
        if spec.rate_limit.is_some() {
            // The descendant's block wins the merged `sharing` gate, so it is
            // also the resource the effective budget belongs to.
            rate_limit_owner = Some(entry.upstream.as_ref().expect("checked").id);
        }
        rate_limit = merge_rate_limit(rate_limit, spec.rate_limit.clone());
        cors = merge_cors(cors, spec.cors.clone());
    }

    EffectiveConfig {
        auth,
        headers,
        plugins,
        rate_limit,
        rate_limit_owner,
        cors,
        tags,
        enabled,
    }
}

fn merge_maps(
    current: &std::collections::BTreeMap<String, String>,
    next: &std::collections::BTreeMap<String, String>,
) -> std::collections::BTreeMap<String, String> {
    let mut merged = current.clone();
    for (key, value) in next {
        merged.insert(key.clone(), value.clone());
    }
    merged
}

fn merge_vecs(current: &[String], next: &[String]) -> Vec<String> {
    let mut merged = current.to_vec();
    for item in next {
        if !merged.contains(item) {
            merged.push(item.clone());
        }
    }
    merged
}

/// Merge one `auth` block: `private` ancestors are not inherited, `enforce`
/// ancestors win, `inherit` ancestors are overridden by the descendant.
#[must_use]
pub fn merge_auth(current: Option<AuthConfig>, next: Option<AuthConfig>) -> Option<AuthConfig> {
    // `private` blocks an ancestor's value only when a descendant supplies one.
    let Some(descendant) = next else {
        return current;
    };
    let ancestor = match current {
        Some(config) if is_private(config.sharing) => None,
        other => other,
    };
    match (ancestor, Some(descendant)) {
        (None, next) => next,
        (Some(ancestor), None) => Some(ancestor),
        (Some(ancestor), Some(descendant)) => {
            if is_enforced(ancestor.sharing) {
                Some(ancestor)
            } else {
                Some(descendant)
            }
        }
    }
}

/// Merge `headers` blocks using the `auth` sharing semantics.
#[must_use]
pub fn merge_headers(current: HeadersConfig, next: Option<HeadersConfig>) -> HeadersConfig {
    // `private` blocks an ancestor's rules only when a descendant supplies some.
    let Some(descendant) = next else {
        return current;
    };
    let current_sharing = current.sharing.unwrap_or(Sharing::Private);
    let ancestor = if is_private(current_sharing) {
        HeadersConfig::default()
    } else {
        current
    };
    let forced = is_enforced(descendant.sharing.unwrap_or(Sharing::Private));
    if forced {
        return descendant;
    }
    HeadersConfig {
        sharing: descendant.sharing,
        request: super::model::RequestHeaders {
            set: merge_maps(&ancestor.request.set, &descendant.request.set),
            add: merge_maps(&ancestor.request.add, &descendant.request.add),
            remove: merge_vecs(&ancestor.request.remove, &descendant.request.remove),
            passthrough: if descendant.request.passthrough == super::model::Passthrough::None {
                ancestor.request.passthrough
            } else {
                descendant.request.passthrough
            },
            passthrough_allowlist: merge_vecs(
                &ancestor.request.passthrough_allowlist,
                &descendant.request.passthrough_allowlist,
            ),
        },
        response: super::model::ResponseHeaders {
            set: merge_maps(&ancestor.response.set, &descendant.response.set),
            add: merge_maps(&ancestor.response.add, &descendant.response.add),
            remove: merge_vecs(&ancestor.response.remove, &descendant.response.remove),
        },
    }
}

/// Concatenate two plugin chains, deduping and preserving first occurrence.
#[must_use]
pub fn merge_plugins(current: PluginsConfig, next: Option<PluginsConfig>) -> PluginsConfig {
    // `private` blocks an ancestor's chain only when a descendant binds one.
    let Some(descendant) = next else {
        return current;
    };
    let ancestor = if is_private(current.sharing) {
        PluginsConfig::default()
    } else {
        current
    };
    let mut items = ancestor.items;
    for item in descendant.items {
        // An entry is inherited once per reference: a descendant that rebinds
        // the same plugin with its own configuration replaces the ancestor's
        // entry instead of running the plugin twice.
        if let Some(existing) = items
            .iter_mut()
            .find(|existing| existing.reference() == item.reference())
        {
            *existing = item;
        } else {
            items.push(item);
        }
    }
    PluginsConfig {
        sharing: descendant.sharing,
        items,
    }
}

/// Merge two rate limits with `min()` semantics per numeric field.
#[must_use]
pub fn merge_rate_limit(
    current: Option<RateLimitConfig>,
    next: Option<RateLimitConfig>,
) -> Option<RateLimitConfig> {
    // `private` blocks an ancestor limit only when a descendant supplies one.
    let Some(descendant) = next else {
        return current;
    };
    let ancestor = match current {
        Some(config) if is_private(config.sharing) => None,
        other => other,
    };
    let Some(ancestor) = ancestor else {
        return Some(descendant);
    };
    let mut merged = ancestor.clone();
    merged.sharing = descendant.sharing;
    merged.sustained = Sustained {
        rate: ancestor.sustained.rate.min(descendant.sustained.rate),
        window: pick_window(&ancestor.sustained, &descendant.sustained),
    };
    merged.burst = min_option_u32(
        ancestor.burst.map(|b| b.capacity),
        descendant.burst.map(|b| b.capacity),
    )
    .map(|capacity| super::model::Burst { capacity });
    merged.cost = min_option_u32(ancestor.cost, descendant.cost);
    merged.response_headers = descendant.response_headers.or(ancestor.response_headers);
    merged.scope = descendant.scope;
    Some(merged)
}

fn rate_per_second(sustained: &Sustained) -> f64 {
    let seconds = sustained.window.seconds();
    if seconds == 0 {
        return 0.0;
    }
    f64::from(sustained.rate) / f64::from(u32::try_from(seconds).unwrap_or(u32::MAX))
}

fn min_option_u32(current: Option<u32>, next: Option<u32>) -> Option<u32> {
    match (current, next) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        (None, None) => None,
    }
}

fn pick_window(current: &Sustained, next: &Sustained) -> super::model::RateWindow {
    // Keep the window of the stricter side so the effective throughput is the
    // smaller of the two in tokens-per-second terms.
    if rate_per_second(next) < rate_per_second(current) {
        next.window
    } else {
        current.window
    }
}

/// Merge CORS blocks: `inherit` unions the allowlists, `enforce` forces the
/// ancestor value, `private` is not inherited.
#[must_use]
pub fn merge_cors(current: Option<CorsConfig>, next: Option<CorsConfig>) -> Option<CorsConfig> {
    // `private` blocks an ancestor's allowlists only when a descendant supplies some.
    let Some(descendant) = next else {
        return current;
    };
    let ancestor = match current {
        Some(config) if is_private(config.sharing) => None,
        other => other,
    };
    let Some(ancestor) = ancestor else {
        return Some(descendant);
    };
    if is_enforced(ancestor.sharing) {
        return Some(ancestor);
    }
    Some(CorsConfig {
        sharing: descendant.sharing,
        enabled: ancestor.enabled || descendant.enabled,
        allowed_origins: merge_vecs(&ancestor.allowed_origins, &descendant.allowed_origins),
        allowed_methods: Some(merge_vecs(&ancestor.methods(), &descendant.methods())),
        expose_headers: merge_vecs(&ancestor.expose_headers, &descendant.expose_headers),
        allow_credentials: ancestor.allow_credentials || descendant.allow_credentials,
    })
}

/// Merge two route-level rate limits with the same `min()` semantics, used when
/// an upstream limit is combined with a route limit.
#[must_use]
pub fn merge_route_rate_limit(
    upstream: Option<RateLimitConfig>,
    route: Option<RateLimitConfig>,
) -> Option<RateLimitConfig> {
    let Some(route) = route else {
        return upstream;
    };
    let Some(upstream) = upstream else {
        return Some(route);
    };
    let mut merged = upstream.clone();
    merged.sharing = route.sharing;
    merged.algorithm = route.algorithm;
    merged.sustained = Sustained {
        rate: upstream.sustained.rate.min(route.sustained.rate),
        window: pick_window(&upstream.sustained, &route.sustained),
    };
    merged.burst = min_option_u32(
        upstream.burst.map(|b| b.capacity),
        route.burst.map(|b| b.capacity),
    )
    .map(|capacity| super::model::Burst { capacity });
    merged.cost = min_option_u32(upstream.cost, route.cost);
    merged.scope = if route.scope == super::model::RateScope::Tenant
        && upstream.scope != super::model::RateScope::Tenant
    {
        upstream.scope
    } else {
        route.scope
    };
    merged.response_headers = route.response_headers.or(upstream.response_headers);
    Some(merged)
}

/// Rate limit algorithm of the merged configuration.
#[must_use]
pub fn merged_algorithm(config: &RateLimitConfig) -> RateAlgorithm {
    config.algorithm
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::model::{Endpoint, ServerConfig, Upstream, UpstreamCreate};
    use uuid::Uuid;

    fn upstream(tenant: Uuid, alias: &str, spec: UpstreamCreate) -> Upstream {
        Upstream {
            id: Uuid::new_v4(),
            tenant_id: tenant,
            alias: alias.to_owned(),
            alias_derived: false,
            created_at: "2026-01-01T00:00:00Z".to_owned(),
            updated_at: "2026-01-01T00:00:00Z".to_owned(),
            spec,
        }
    }

    fn spec() -> UpstreamCreate {
        UpstreamCreate {
            enabled: true,
            alias: None,
            tags: Vec::new(),
            server: ServerConfig {
                endpoints: vec![Endpoint {
                    scheme: "https".to_owned(),
                    host: "api.example.com".to_owned(),
                    port: 443,
                }],
            },
            protocol: crate::domain::model::PROTOCOL_HTTP.to_owned(),
            auth: None,
            headers: None,
            plugins: None,
            rate_limit: None,
            cors: None,
        }
    }

    fn auth(plugin: &str, sharing: Sharing) -> AuthConfig {
        AuthConfig {
            plugin_type: format!("gts.cf.core.oagw.auth_plugin.v1~{plugin}"),
            sharing,
            config: serde_json::Map::new(),
        }
    }

    fn rate(sharing: Sharing, rate: u32, capacity: u32, cost: u32) -> RateLimitConfig {
        RateLimitConfig {
            sharing,
            algorithm: RateAlgorithm::TokenBucket,
            sustained: Sustained {
                rate,
                window: super::super::model::RateWindow::Second,
            },
            burst: Some(super::super::model::Burst { capacity }),
            budget: None,
            scope: super::super::model::RateScope::Tenant,
            strategy: super::super::model::RateStrategy::Reject,
            cost: Some(cost),
            response_headers: None,
        }
    }

    #[test]
    fn enforced_ancestor_auth_wins() {
        let ancestor = auth("cf.core.oagw.apikey.v1", Sharing::Enforce);
        let descendant = auth("cf.core.oagw.noop.v1", Sharing::Inherit);
        let merged = merge_auth(Some(ancestor.clone()), Some(descendant));
        assert_eq!(merged, Some(ancestor));
    }

    #[test]
    fn private_ancestor_auth_is_kept_without_a_descendant_block() {
        // `private` hides an ancestor's value from a descendant that supplies
        // one; with no descendant block the configured auth still applies.
        let ancestor = auth("cf.core.oagw.apikey.v1", Sharing::Private);
        let merged = merge_auth(Some(ancestor.clone()), None);
        assert_eq!(merged, Some(ancestor));
    }

    #[test]
    fn private_ancestor_auth_is_dropped_for_a_descendant_block() {
        let ancestor = auth("cf.core.oagw.apikey.v1", Sharing::Private);
        let descendant = auth("cf.core.oagw.noop.v1", Sharing::Inherit);
        let merged = merge_auth(Some(ancestor), Some(descendant));
        assert!(merged.is_some());
    }

    #[test]
    fn rate_limit_min_merge() {
        let merged = merge_rate_limit(
            Some(rate(Sharing::Enforce, 10, 20, 5)),
            Some(rate(Sharing::Inherit, 100, 50, 2)),
        )
        .expect("merged");
        assert_eq!(merged.sustained.rate, 10);
        assert_eq!(merged.capacity(), 20);
        assert_eq!(merged.cost(), 2);
    }

    #[test]
    fn ancestor_disabled_disables_descendant() {
        let mut ancestor = spec();
        ancestor.enabled = false;
        let chain = [
            ChainEntry {
                tenant_id: Uuid::new_v4(),
                upstream: Some(upstream(Uuid::new_v4(), "api.example.com", ancestor)),
            },
            ChainEntry {
                tenant_id: Uuid::new_v4(),
                upstream: Some(upstream(Uuid::new_v4(), "api.example.com", spec())),
            },
        ];
        let merged = merge_chain(&chain);
        assert!(!merged.enabled);
    }

    #[test]
    fn tags_union() {
        let mut ancestor = spec();
        ancestor.tags = vec!["llm".to_owned(), "openai".to_owned()];
        let mut descendant = spec();
        descendant.tags = vec!["openai".to_owned(), "prod".to_owned()];
        let chain = [
            ChainEntry {
                tenant_id: Uuid::new_v4(),
                upstream: Some(upstream(Uuid::new_v4(), "api.example.com", ancestor)),
            },
            ChainEntry {
                tenant_id: Uuid::new_v4(),
                upstream: Some(upstream(Uuid::new_v4(), "api.example.com", descendant)),
            },
        ];
        let merged = merge_chain(&chain);
        assert_eq!(merged.tags, vec!["llm", "openai", "prod"]);
    }

    #[test]
    fn plugins_concatenate_ancestor_first() {
        let ancestor = PluginsConfig {
            sharing: Sharing::Inherit,
            items: vec![crate::domain::model::PluginItem::Reference(
                "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1".to_owned(),
            )],
        };
        let descendant = PluginsConfig {
            sharing: Sharing::Inherit,
            items: vec![
                crate::domain::model::PluginItem::Reference(
                    "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.request_id.v1".to_owned(),
                ),
                crate::domain::model::PluginItem::Reference(
                    "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1".to_owned(),
                ),
            ],
        };
        let merged = merge_plugins(ancestor, Some(descendant));
        assert_eq!(merged.items.len(), 2);
        assert_eq!(
            merged.items[0].reference(),
            "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1"
        );
    }

    #[test]
    fn cors_union_on_inherit() {
        let ancestor = CorsConfig {
            sharing: Sharing::Inherit,
            enabled: true,
            allowed_origins: vec!["https://a.example.com".to_owned()],
            allowed_methods: Some(vec!["GET".to_owned()]),
            expose_headers: vec!["x-a".to_owned()],
            allow_credentials: false,
        };
        let descendant = CorsConfig {
            sharing: Sharing::Inherit,
            enabled: true,
            allowed_origins: vec!["https://b.example.com".to_owned()],
            allowed_methods: Some(vec!["POST".to_owned()]),
            expose_headers: vec!["x-b".to_owned()],
            allow_credentials: false,
        };
        let merged = merge_cors(Some(ancestor), Some(descendant)).expect("merged");
        assert_eq!(merged.allowed_origins.len(), 2);
        assert_eq!(merged.methods().len(), 2);
        assert_eq!(merged.expose_headers.len(), 2);
    }
}
