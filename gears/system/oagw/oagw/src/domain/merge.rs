//! Hierarchical configuration merge (DESIGN "Hierarchical Configuration").
//!
//! Upstream < Route < Tenant merge priority; `min(ancestor, descendant)` for
//! rate limits; concatenation for plugin chains; union for CORS origins.

use crate::domain::model::{CorsConfig, PluginListConfig, RateLimitConfig, Sharing};

/// Merges two plugin chains: ancestor items first, then descendant items.
///
/// `enforce` on the ancestor blocks any descendant override.
#[must_use]
pub fn merge_plugins(ancestor: Option<&PluginListConfig>, descendant: Option<&PluginListConfig>) -> PluginListConfig {
    match (ancestor, descendant) {
        (Some(a), Some(d)) => {
            let mut items = a.items.clone();
            for item in &d.items {
                if items.iter().any(|bound| bound.id() == item.id()) {
                    continue;
                }
                items.push(item.clone());
            }
            PluginListConfig {
                sharing: if a.sharing == Sharing::Enforce { a.sharing } else { d.sharing },
                items,
            }
        }
        (Some(a), None) => a.clone(),
        (None, Some(d)) => d.clone(),
        (None, None) => PluginListConfig::default(),
    }
}

/// Merges rate limits: the stricter (lower) effective limit wins.
///
/// `private` ancestor limits are not inherited; `inherit`/`enforce` are.
#[must_use]
pub fn merge_rate_limit(
    ancestor: Option<&RateLimitConfig>,
    descendant: Option<&RateLimitConfig>,
) -> Option<RateLimitConfig> {
    match (ancestor, descendant) {
        (Some(a), Some(d)) => {
            if a.sharing == Sharing::Private {
                return Some(d.clone());
            }
            let capacity = a.capacity().min(d.capacity());
            let sustained_rate = a.sustained.rate.min(d.sustained.rate);
            Some(RateLimitConfig {
                sharing: if a.sharing == Sharing::Enforce { a.sharing } else { d.sharing },
                algorithm: d.algorithm,
                sustained: crate::domain::model::SustainedRate {
                    rate: sustained_rate,
                    window: d.sustained.window,
                },
                burst: Some(crate::domain::model::BurstCapacity { capacity }),
                scope: d.scope,
                strategy: d.strategy,
                cost: d.cost,
                response_headers: d.response_headers,
            })
        }
        (Some(a), None) => (a.sharing != Sharing::Private).then(|| a.clone()),
        (None, Some(d)) => Some(d.clone()),
        (None, None) => None,
    }
}

/// Merges CORS configuration: union of origins/methods when the ancestor
/// allows inheritance, otherwise the descendant's own configuration.
#[must_use]
pub fn merge_cors(ancestor: Option<&CorsConfig>, descendant: Option<&CorsConfig>) -> Option<CorsConfig> {
    match (ancestor, descendant) {
        (Some(a), Some(d)) => {
            if a.sharing == Sharing::Private || !a.enabled {
                return Some(d.clone());
            }
            let mut merged = d.clone();
            for origin in &a.allowed_origins {
                if !merged.allowed_origins.contains(origin) {
                    merged.allowed_origins.push(origin.clone());
                }
            }
            for method in &a.allowed_methods {
                if !merged.allowed_methods.contains(method) {
                    merged.allowed_methods.push(method.clone());
                }
            }
            for header in &a.expose_headers {
                if !merged.expose_headers.contains(header) {
                    merged.expose_headers.push(header.clone());
                }
            }
            merged.allow_credentials = a.allow_credentials && d.allow_credentials;
            merged.sharing = if a.sharing == Sharing::Enforce { a.sharing } else { d.sharing };
            Some(merged)
        }
        (Some(a), None) => (a.sharing != Sharing::Private).then(|| a.clone()),
        (None, Some(d)) => Some(d.clone()),
        (None, None) => None,
    }
}

/// Union of tags across the tenant chain (descendants can add, not remove).
#[must_use]
pub fn merge_tags(ancestor: &[String], descendant: &[String]) -> Vec<String> {
    let mut tags = ancestor.to_vec();
    for tag in descendant {
        if !tags.contains(tag) {
            tags.push(tag.clone());
        }
    }
    tags
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::model::PluginBinding;

    fn rate(rate: u64, capacity: u64, sharing: Sharing) -> RateLimitConfig {
        RateLimitConfig {
            sharing,
            sustained: crate::domain::model::SustainedRate {
                rate,
                window: crate::domain::model::RateWindow::Minute,
            },
            burst: Some(crate::domain::model::BurstCapacity { capacity }),
            ..RateLimitConfig::default()
        }
    }

    #[test]
    fn stricter_rate_limit_wins() {
        let ancestor = rate(10_000, 1_000, Sharing::Inherit);
        let descendant = rate(1_000, 100, Sharing::Private);
        let merged = merge_rate_limit(Some(&ancestor), Some(&descendant)).unwrap();
        assert_eq!(merged.sustained.rate, 1_000);
        assert_eq!(merged.capacity(), 100);
    }

    #[test]
    fn private_ancestor_rate_limit_is_not_inherited() {
        let ancestor = rate(10_000, 1_000, Sharing::Private);
        let descendant = rate(1_000, 100, Sharing::Private);
        let merged = merge_rate_limit(Some(&ancestor), Some(&descendant)).unwrap();
        assert_eq!(merged.sustained.rate, 1_000);
    }

    #[test]
    fn inherited_only_uses_ancestor() {
        let ancestor = rate(500, 500, Sharing::Inherit);
        let merged = merge_rate_limit(Some(&ancestor), None).unwrap();
        assert_eq!(merged.sustained.rate, 500);
    }

    #[test]
    fn private_ancestor_without_descendant_is_dropped() {
        let ancestor = rate(500, 500, Sharing::Private);
        assert!(merge_rate_limit(Some(&ancestor), None).is_none());
    }

    #[test]
    fn plugin_chains_concatenate() {
        let ancestor = PluginListConfig {
            sharing: Sharing::Inherit,
            items: vec![binding("a"), binding("b")],
        };
        let descendant = PluginListConfig {
            sharing: Sharing::Private,
            items: vec![binding("b"), binding("c")],
        };
        let merged = merge_plugins(Some(&ancestor), Some(&descendant));
        assert_eq!(merged.items, vec![binding("a"), binding("b"), binding("c")]);
    }

    #[test]
    fn plugin_chains_dedup_by_identifier_not_by_config() {
        let ancestor = PluginListConfig {
            sharing: Sharing::Inherit,
            items: vec![binding("a")],
        };
        // The same plugin bound twice, once bare and once with config.
        let descendant = PluginListConfig {
            sharing: Sharing::Private,
            items: vec![PluginBinding::Bound {
                plugin_ref: "a".to_owned(),
                config: serde_json::json!({"required_request_headers": "x-trace-id"}),
            }],
        };
        // The ancestor binding wins: upstream bindings execute before route ones.
        let merged = merge_plugins(Some(&ancestor), Some(&descendant));
        assert_eq!(merged.items, vec![binding("a")]);
    }

    fn binding(id: &str) -> PluginBinding {
        PluginBinding::Ref(id.to_owned())
    }

    #[test]
    fn cors_unions_origins() {
        let ancestor = CorsConfig {
            sharing: Sharing::Inherit,
            enabled: true,
            allowed_origins: vec!["https://a.example.com".to_owned()],
            ..CorsConfig::default()
        };
        let descendant = CorsConfig {
            enabled: true,
            allowed_origins: vec!["https://b.example.com".to_owned()],
            ..CorsConfig::default()
        };
        let merged = merge_cors(Some(&ancestor), Some(&descendant)).unwrap();
        assert_eq!(merged.allowed_origins.len(), 2);
    }
}
