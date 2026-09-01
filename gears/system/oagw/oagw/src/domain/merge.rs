//! Hierarchical configuration merge (DESIGN §3.2 "Hierarchical Configuration").
//!
//! Effective configuration is computed by walking the tenant chain from root to
//! descendant and folding each field according to its sharing mode:
//!
//! | Field        | Merge strategy                                    |
//! |--------------|---------------------------------------------------|
//! | Auth         | Override if `inherit`; forced if `enforce`        |
//! | Rate limits  | `min(ancestor, descendant)` — stricter always wins|
//! | Plugins      | Concatenate: `ancestor.plugins + descendant.plugins` |
//! | CORS         | Union origins if `inherit`; forced if `enforce`   |
//! | Headers      | Set/add tables unioned, remove lists concatenated |
//! | Tags         | Add-only union (no sharing mode)                  |
//!
//! Shadowing selects the routing target only: an ancestor field configured with
//! `sharing: enforce` is never bypassed by the descendant's own value.

use crate::domain::model::{
    AuthConfig, CorsConfig, HeadersConfig, PluginsConfig, RateLimitConfig, Sharing,
};

/// Merges two auth configurations according to the descendant's sharing mode.
///
/// A level that declares no authentication of its own (`auth` omitted, which
/// deserialises as the noop default) rides the chain: an ancestor that shares
/// or enforces its authentication is not silently replaced by the noop, while a
/// private ancestor stays invisible to the descendant.
///
/// `enforce` on the ancestor pins the merged value; any other mode lets the
/// descendant override with its own block.
#[must_use]
pub fn merge_auth(ancestor: &AuthConfig, descendant: &AuthConfig) -> AuthConfig {
    if descendant.is_unspecified() {
        return if ancestor.sharing == Sharing::Private {
            AuthConfig::default()
        } else {
            ancestor.clone()
        };
    }
    if ancestor.sharing == Sharing::Enforce {
        // Shadowing selects the routing target only; an enforced ancestor
        // constraint is never bypassed (DESIGN §3.2).
        return ancestor.clone();
    }
    descendant.clone()
}

/// Merges rate limits: the stricter (smaller) of the two always wins.
#[must_use]
pub fn merge_rate_limit(
    ancestor: Option<&RateLimitConfig>,
    descendant: Option<&RateLimitConfig>,
) -> Option<RateLimitConfig> {
    match (ancestor, descendant) {
        (None, None) => None,
        (Some(only), None) | (None, Some(only)) => Some(only.clone()),
        (Some(parent), Some(child)) => Some(RateLimitConfig {
            sharing: stricter_sharing(parent.sharing, child.sharing),
            // Stricter (smaller) rate and the wider window it belongs to.
            sustained: pick_sustained(&parent.sustained, &child.sustained),
            burst: pick_burst(&parent.burst, &child.burst),
            scope: child.scope,
            algorithm: if parent.algorithm == child.algorithm {
                parent.algorithm
            } else {
                child.algorithm
            },
            strategy: if parent.strategy == child.strategy {
                parent.strategy
            } else {
                child.strategy
            },
            cost: parent.cost.max(child.cost),
            response_headers: parent.response_headers && child.response_headers,
        }),
    }
}

/// Picks the stricter sustained rate, comparing the two limits as requests per
/// second so different windows are comparable; the winning limit keeps its own
/// window.
fn pick_sustained(
    parent: &crate::domain::model::SustainedRate,
    child: &crate::domain::model::SustainedRate,
) -> crate::domain::model::SustainedRate {
    let parent_per_second = per_second(parent);
    let child_per_second = per_second(child);
    let (rate, window) = if parent_per_second <= child_per_second {
        (parent.rate, parent.window)
    } else {
        (child.rate, child.window)
    };
    crate::domain::model::SustainedRate { rate, window }
}

/// Normalises a sustained rate to requests per second.
fn per_second(rate: &crate::domain::model::SustainedRate) -> f64 {
    let seconds = rate.window.seconds().max(1) as f64;
    rate.rate as f64 / seconds
}

/// Picks the stricter burst capacity; an unset capacity stays unset.
fn pick_burst(
    parent: &crate::domain::model::BurstConfig,
    child: &crate::domain::model::BurstConfig,
) -> crate::domain::model::BurstConfig {
    crate::domain::model::BurstConfig {
        capacity: match (parent.capacity, child.capacity) {
            (Some(parent), Some(child)) => Some(parent.min(child)),
            _ => None,
        },
    }
}

fn stricter_sharing(parent: Sharing, child: Sharing) -> Sharing {
    match (parent, child) {
        (Sharing::Enforce, _) | (_, Sharing::Enforce) => Sharing::Enforce,
        (Sharing::Inherit, _) | (_, Sharing::Inherit) => Sharing::Inherit,
        _ => Sharing::Private,
    }
}

/// Concatenates plugin chains: `ancestor.plugins + descendant.plugins`.
///
/// Position is normalised to be contiguous from zero after the merge.
#[must_use]
pub fn merge_plugins(ancestor: &PluginsConfig, descendant: &PluginsConfig) -> PluginsConfig {
    let mut items = ancestor.items.clone();
    items.extend(descendant.items.clone());
    PluginsConfig {
        sharing: if ancestor.sharing == Sharing::Enforce || descendant.sharing == Sharing::Enforce {
            Sharing::Enforce
        } else {
            descendant.sharing
        },
        items,
    }
}

/// Unions the `set`/`add` header tables and concatenates the removal lists.
#[must_use]
pub fn merge_headers(ancestor: &HeadersConfig, descendant: &HeadersConfig) -> HeadersConfig {
    let mut request_set = ancestor.request.set.clone();
    for (key, value) in &descendant.request.set {
        request_set.insert(key.clone(), value.clone());
    }
    let mut request_add = ancestor.request.add.clone();
    for (key, value) in &descendant.request.add {
        request_add.insert(key.clone(), value.clone());
    }
    let mut request_remove = ancestor.request.remove.clone();
    request_remove.extend(descendant.request.remove.iter().cloned());

    let mut response_set = ancestor.response.set.clone();
    for (key, value) in &descendant.response.set {
        response_set.insert(key.clone(), value.clone());
    }
    let mut response_add = ancestor.response.add.clone();
    for (key, value) in &descendant.response.add {
        response_add.insert(key.clone(), value.clone());
    }
    let mut response_remove = ancestor.response.remove.clone();
    response_remove.extend(descendant.response.remove.iter().cloned());

    HeadersConfig {
        request: crate::domain::model::RequestHeaders {
            set: request_set,
            add: request_add,
            remove: request_remove,
            passthrough: if descendant.request.passthrough
                == crate::domain::model::Passthrough::None
            {
                ancestor.request.passthrough
            } else {
                descendant.request.passthrough
            },
            passthrough_allowlist: if descendant.request.passthrough_allowlist.is_empty() {
                ancestor.request.passthrough_allowlist.clone()
            } else {
                descendant.request.passthrough_allowlist.clone()
            },
        },
        response: crate::domain::model::ResponseHeaders {
            set: response_set,
            add: response_add,
            remove: response_remove,
        },
    }
}

/// Merges CORS configurations (ADR-0004).
///
/// Origins are unioned when the descendant inherits; `enforce` pins the
/// ancestor policy so a descendant cannot widen the origin set.
#[must_use]
pub fn merge_cors(
    ancestor: Option<&CorsConfig>,
    descendant: Option<&CorsConfig>,
) -> Option<CorsConfig> {
    let Some(descendant) = descendant else {
        return ancestor.cloned();
    };
    let Some(ancestor) = ancestor else {
        return Some(descendant.clone());
    };
    if ancestor.sharing == Sharing::Enforce {
        return Some(ancestor.clone());
    }
    if descendant.sharing == Sharing::Private {
        return Some(descendant.clone());
    }
    let mut merged = descendant.clone();
    for origin in &ancestor.allowed_origins {
        if !merged.allowed_origins.contains(origin) {
            merged.allowed_origins.push(origin.clone());
        }
    }
    for method in &ancestor.allowed_methods {
        if !merged.allowed_methods.contains(method) {
            merged.allowed_methods.push(method.clone());
        }
    }
    for header in &ancestor.expose_headers {
        if !merged.expose_headers.contains(header) {
            merged.expose_headers.push(header.clone());
        }
    }
    merged.allow_credentials = ancestor.allow_credentials && descendant.allow_credentials;
    merged.enabled = ancestor.enabled || descendant.enabled;
    // A unioned policy that keeps credentials must not reintroduce the
    // wildcard origin (ADR-0004), and an inherited policy cannot widen a
    // descendant that is disabled.
    if merged.allowed_origins.iter().any(|origin| origin == "*") && merged.allow_credentials {
        merged.allow_credentials = false;
    }
    Some(merged)
}

/// Add-only tag union: descendants cannot remove inherited tags.
#[must_use]
pub fn merge_tags(ancestor: &[String], descendant: &[String]) -> Vec<String> {
    let mut merged = ancestor.to_vec();
    for tag in descendant {
        if !merged.contains(tag) {
            merged.push(tag.clone());
        }
    }
    merged
}

/// `true` when the ancestor level may not be overridden at all.
#[must_use]
pub fn is_enforced(sharing: Sharing) -> bool {
    sharing == Sharing::Enforce
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::model::{
        BurstConfig, RateAlgorithm, RateScope, RateStrategy, RequestHeaders, ResponseHeaders,
        SustainedRate, Window,
    };

    fn auth(sharing: Sharing, plugin: &str) -> AuthConfig {
        AuthConfig {
            plugin_type: Some(plugin.to_owned()),
            sharing,
            config: serde_json::json!({}),
        }
    }

    fn limit(rate: u64, window: Window) -> RateLimitConfig {
        RateLimitConfig {
            sharing: Sharing::Private,
            algorithm: RateAlgorithm::TokenBucket,
            sustained: SustainedRate { rate, window },
            burst: BurstConfig { capacity: None },
            scope: RateScope::Tenant,
            strategy: RateStrategy::Reject,
            cost: 1,
            response_headers: true,
        }
    }

    fn headers() -> HeadersConfig {
        HeadersConfig {
            request: RequestHeaders::default(),
            response: ResponseHeaders::default(),
        }
    }

    #[test]
    fn enforced_ancestor_auth_wins() {
        let ancestor = auth(Sharing::Enforce, "ancestor");
        let descendant = auth(Sharing::Inherit, "descendant");
        assert_eq!(
            merge_auth(&ancestor, &descendant).plugin_type.as_deref(),
            Some("ancestor")
        );
    }

    #[test]
    fn inherited_descendant_auth_wins() {
        let ancestor = auth(Sharing::Inherit, "ancestor");
        let descendant = auth(Sharing::Inherit, "descendant");
        assert_eq!(
            merge_auth(&ancestor, &descendant).plugin_type.as_deref(),
            Some("descendant")
        );
    }

    #[test]
    fn private_descendant_keeps_its_own_auth() {
        let ancestor = auth(Sharing::Inherit, "ancestor");
        let descendant = auth(Sharing::Private, "descendant");
        // `private` hides the ancestor level entirely.
        assert_eq!(
            merge_auth(&ancestor, &descendant).plugin_type.as_deref(),
            Some("descendant")
        );
    }

    #[test]
    fn enforced_ancestor_auth_beats_a_private_descendant() {
        // Shadowing selects the routing target only; an enforced ancestor
        // constraint is never bypassed (DESIGN §3.2).
        let ancestor = auth(Sharing::Enforce, "ancestor");
        let descendant = auth(Sharing::Private, "descendant");
        assert_eq!(
            merge_auth(&ancestor, &descendant).plugin_type.as_deref(),
            Some("ancestor")
        );
    }

    #[test]
    fn stricter_rate_limit_wins() {
        let merged = merge_rate_limit(
            Some(&limit(100, Window::Minute)),
            Some(&limit(40, Window::Minute)),
        )
        .expect("merged");
        assert_eq!(merged.sustained.rate, 40);
    }

    #[test]
    fn missing_rate_limit_falls_back_to_the_other_side() {
        assert!(merge_rate_limit(None, None).is_none());
        assert_eq!(
            merge_rate_limit(Some(&limit(5, Window::Second)), None)
                .expect("merged")
                .sustained
                .rate,
            5
        );
    }

    #[test]
    fn burst_capacity_takes_the_smaller_side() {
        let mut parent = limit(100, Window::Minute);
        parent.burst = BurstConfig { capacity: Some(50) };
        let mut child = limit(40, Window::Minute);
        child.burst = BurstConfig {
            capacity: Some(200),
        };
        let merged = merge_rate_limit(Some(&parent), Some(&child)).expect("merged");
        assert_eq!(merged.burst.capacity, Some(50));

        child.burst = BurstConfig { capacity: None };
        let merged = merge_rate_limit(Some(&parent), Some(&child)).expect("merged");
        // An unset capacity is unset, not zero.
        assert_eq!(merged.burst.capacity, None);
    }

    #[test]
    fn sustained_rates_are_compared_per_second() {
        // 120/minute (2/s) is stricter than 10/second, so the parent wins.
        let merged = merge_rate_limit(
            Some(&limit(120, Window::Minute)),
            Some(&limit(10, Window::Second)),
        )
        .expect("merged");
        assert_eq!(merged.sustained.rate, 120);
        assert_eq!(merged.sustained.window, Window::Minute);
    }

    #[test]
    fn an_unspecified_descendant_auth_inherits_the_ancestor() {
        // A level that omits `auth` deserialises as the noop default; it must
        // not silently replace an inherited ancestor plugin.
        let ancestor = auth(Sharing::Inherit, "ancestor");
        let merged = merge_auth(&ancestor, &AuthConfig::default());
        assert_eq!(merged.plugin_type.as_deref(), Some("ancestor"));
    }

    #[test]
    fn credentialled_cors_never_keeps_a_wildcard_origin() {
        let ancestor = CorsConfig {
            sharing: Sharing::Inherit,
            allowed_origins: vec!["https://a.example".to_owned()],
            allow_credentials: true,
            ..CorsConfig::default()
        };
        let descendant = CorsConfig {
            sharing: Sharing::Inherit,
            allowed_origins: vec!["*".to_owned()],
            allow_credentials: true,
            ..CorsConfig::default()
        };
        let merged = merge_cors(Some(&ancestor), Some(&descendant)).expect("merged");
        assert!(!merged.allow_credentials);
    }

    #[test]
    fn plugin_chains_are_concatenated() {
        let ancestor = PluginsConfig {
            items: vec![crate::domain::model::PluginBinding::from_ref("a")],
            ..PluginsConfig::default()
        };
        let descendant = PluginsConfig {
            items: vec![crate::domain::model::PluginBinding::from_ref("b")],
            ..PluginsConfig::default()
        };
        let merged = merge_plugins(&ancestor, &descendant);
        assert_eq!(merged.items.len(), 2);
        assert_eq!(merged.items[0].plugin_ref, "a");
        assert_eq!(merged.items[1].plugin_ref, "b");
    }

    #[test]
    fn header_tables_union_and_removals_concatenate() {
        let mut ancestor = headers();
        ancestor
            .request
            .set
            .insert("x-a".to_owned(), "1".to_owned());
        ancestor.request.remove.push("x-drop".to_owned());
        let mut descendant = headers();
        descendant
            .request
            .set
            .insert("x-b".to_owned(), "2".to_owned());
        descendant.request.remove.push("x-drop2".to_owned());
        let merged = merge_headers(&ancestor, &descendant);
        assert_eq!(merged.request.set.len(), 2);
        assert_eq!(merged.request.remove.len(), 2);
    }

    #[test]
    fn cors_origins_are_unioned_on_inherit() {
        let ancestor = CorsConfig {
            sharing: Sharing::Inherit,
            allowed_origins: vec!["https://a.example".to_owned()],
            ..CorsConfig::default()
        };
        let descendant = CorsConfig {
            sharing: Sharing::Inherit,
            allowed_origins: vec!["https://b.example".to_owned()],
            ..CorsConfig::default()
        };
        let merged = merge_cors(Some(&ancestor), Some(&descendant)).expect("merged");
        assert_eq!(merged.allowed_origins.len(), 2);
    }

    #[test]
    fn enforced_cors_is_pinned() {
        let ancestor = CorsConfig {
            sharing: Sharing::Enforce,
            allowed_origins: vec!["https://a.example".to_owned()],
            ..CorsConfig::default()
        };
        let descendant = CorsConfig {
            allowed_origins: vec!["https://evil.example".to_owned()],
            ..CorsConfig::default()
        };
        let merged = merge_cors(Some(&ancestor), Some(&descendant)).expect("merged");
        assert_eq!(merged.allowed_origins, vec!["https://a.example".to_owned()]);
    }

    #[test]
    fn tags_are_add_only_union() {
        let merged = merge_tags(
            &["shared".to_owned()],
            &["shared".to_owned(), "own".to_owned()],
        );
        assert_eq!(merged, vec!["shared".to_owned(), "own".to_owned()]);
    }

    #[test]
    fn enforced_sharing_is_detected() {
        assert!(is_enforced(Sharing::Enforce));
        assert!(!is_enforced(Sharing::Inherit));
        assert!(!is_enforced(Sharing::Private));
    }
}
