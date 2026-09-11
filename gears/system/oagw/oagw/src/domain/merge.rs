//! Hierarchical configuration merge.
//!
//! `cpt-cf-oagw-fr-hierarchical-config` and `docs/DESIGN.md`
//! §"Hierarchical Configuration" define one merge strategy per field:
//!
//! | Field       | Strategy                                        |
//! |-------------|-------------------------------------------------|
//! | Auth        | override if `inherit`; forced if `enforce`      |
//! | Rate limits | `min(ancestor, descendant)` — stricter wins     |
//! | Plugins     | concatenate ancestor chain then descendant      |
//! | CORS        | union origins if `inherit`; forced if `enforce` |
//! | Tags        | add-only union (no sharing mode)                |
//!
//! Inputs arrive ordered **root first, selected upstream last**, matching the
//! ancestor chain returned by the tenant resolver reversed.

use std::collections::BTreeSet;

use super::model::{
    AuthConfig, CorsConfig, PluginBinding, PluginsConfig, RateLimitConfig, SharingMode,
};

/// Merge a layered auth configuration.
///
/// An `enforce` ancestor wins outright; otherwise the closest layer that
/// declares a plugin wins, with `private` ancestors invisible to descendants.
#[must_use]
pub fn merge_auth(layers: &[&AuthConfig]) -> Option<AuthConfig> {
    // Ancestors are all layers except the last (the selected upstream).
    let (own, ancestors) = layers.split_last()?;

    if let Some(enforced) = ancestors
        .iter()
        .find(|a| a.sharing.is_enforced() && a.plugin_ref.is_some())
    {
        return Some((*enforced).clone());
    }
    if own.plugin_ref.is_some() {
        return Some((*own).clone());
    }
    // Nothing of our own: fall back to the closest visible ancestor.
    ancestors
        .iter()
        .rev()
        .find(|a| a.sharing.is_visible() && a.plugin_ref.is_some())
        .map(|a| (*a).clone())
}

/// Merge layered rate limits: the effective limit is the strictest across
/// every layer that is visible to the caller, plus the caller's own.
#[must_use]
pub fn merge_rate_limit(layers: &[&RateLimitConfig]) -> Option<RateLimitConfig> {
    let (own, ancestors) = layers.split_last()?;
    let mut effective = (*own).clone();

    for ancestor in ancestors.iter().filter(|a| a.sharing.is_visible()) {
        // Stricter always wins, expressed in tokens per second so windows
        // of different units compare correctly.
        if ancestor.sustained.per_second() < effective.sustained.per_second() {
            effective.sustained = ancestor.sustained;
        }
        let capacity = effective.capacity().min(ancestor.capacity());
        effective.burst.capacity = Some(capacity);
        // An enforcing ancestor also pins the algorithm and strategy so a
        // descendant cannot soften enforcement by switching to `degrade`.
        if ancestor.sharing.is_enforced() {
            effective.algorithm = ancestor.algorithm;
            effective.strategy = ancestor.strategy;
            effective.cost = effective.cost.max(ancestor.cost);
        }
    }
    Some(effective)
}

/// Concatenate plugin chains: ancestor bindings run before descendant ones.
///
/// `private` ancestors contribute nothing. Bindings from an `enforce`
/// ancestor cannot be removed by a descendant, which falls out of
/// concatenation — a descendant can only append.
#[must_use]
pub fn merge_plugins(layers: &[&PluginsConfig]) -> PluginsConfig {
    let Some((own, ancestors)) = layers.split_last() else {
        return PluginsConfig::default();
    };
    let mut items: Vec<PluginBinding> = Vec::new();
    let mut seen: BTreeSet<String> = BTreeSet::new();

    for ancestor in ancestors.iter().filter(|a| a.sharing.is_visible()) {
        for binding in &ancestor.items {
            if seen.insert(binding.plugin_ref.clone()) {
                items.push(binding.clone());
            }
        }
    }
    for binding in &own.items {
        if seen.insert(binding.plugin_ref.clone()) {
            items.push(binding.clone());
        }
    }

    PluginsConfig {
        sharing: own.sharing,
        items,
    }
}

/// Merge layered CORS policies.
///
/// An `enforce` ancestor is used verbatim. Otherwise `inherit` ancestors
/// union their origins, methods and exposed headers into the descendant's
/// policy; `allow_credentials` requires agreement from every contributing
/// layer.
#[must_use]
pub fn merge_cors(layers: &[&CorsConfig]) -> Option<CorsConfig> {
    let (own, ancestors) = layers.split_last()?;

    if let Some(enforced) = ancestors.iter().find(|a| a.sharing.is_enforced()) {
        return Some((*enforced).clone());
    }

    let mut effective = (*own).clone();
    for ancestor in ancestors.iter().filter(|a| a.sharing.is_visible()) {
        effective.enabled = effective.enabled || ancestor.enabled;
        union_into(&mut effective.allowed_origins, &ancestor.allowed_origins);
        union_into(&mut effective.allowed_methods, &ancestor.allowed_methods);
        union_into(&mut effective.expose_headers, &ancestor.expose_headers);
        effective.allow_credentials = effective.allow_credentials && ancestor.allow_credentials;
    }
    Some(effective)
}

/// Add-only union of discovery tags: descendants add, never remove.
#[must_use]
pub fn merge_tags(layers: &[&Vec<String>]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for layer in layers {
        union_into(&mut out, layer);
    }
    out.sort();
    out
}

/// Whether an upstream is effectively enabled: an ancestor disabling it wins
/// over any descendant re-enabling it (`cpt-cf-oagw-fr-enable-disable`).
#[must_use]
pub fn effective_enabled(chain_root_first: &[bool]) -> bool {
    chain_root_first.iter().all(|enabled| *enabled)
}

fn union_into(target: &mut Vec<String>, extra: &[String]) {
    for value in extra {
        if !target.iter().any(|existing| existing == value) {
            target.push(value.clone());
        }
    }
}

/// Sharing-mode gate for a single optional field, used where the merge is a
/// plain "closest visible layer wins".
#[must_use]
pub fn visible_to_descendant(sharing: SharingMode) -> bool {
    sharing.is_visible()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::model::{
        BurstConfig, RateAlgorithm, RateScope, RateStrategy, RateWindow, SustainedRate,
    };

    fn limit(rate: u32, window: RateWindow, sharing: SharingMode) -> RateLimitConfig {
        RateLimitConfig {
            sharing,
            algorithm: RateAlgorithm::TokenBucket,
            sustained: SustainedRate { rate, window },
            burst: BurstConfig { capacity: None },
            budget: None,
            scope: RateScope::Tenant,
            strategy: RateStrategy::Reject,
            cost: 1,
            response_headers: true,
        }
    }

    fn auth(plugin: Option<&str>, sharing: SharingMode) -> AuthConfig {
        AuthConfig {
            plugin_ref: plugin.map(str::to_owned),
            sharing,
            config: serde_json::Map::new(),
        }
    }

    #[test]
    fn descendant_rate_limit_can_only_be_stricter() {
        let root = limit(10_000, RateWindow::Minute, SharingMode::Enforce);
        let leaf = limit(100, RateWindow::Minute, SharingMode::Private);
        let merged = merge_rate_limit(&[&root, &leaf]).expect("merged");
        assert_eq!(merged.sustained.rate, 100);
    }

    #[test]
    fn enforced_ancestor_caps_a_looser_descendant() {
        let root = limit(1_000, RateWindow::Minute, SharingMode::Enforce);
        let leaf = limit(50_000, RateWindow::Minute, SharingMode::Private);
        let merged = merge_rate_limit(&[&root, &leaf]).expect("merged");
        assert_eq!(merged.sustained.rate, 1_000);
    }

    #[test]
    fn private_ancestor_rate_limit_is_invisible() {
        let root = limit(10, RateWindow::Second, SharingMode::Private);
        let leaf = limit(1_000, RateWindow::Second, SharingMode::Private);
        let merged = merge_rate_limit(&[&root, &leaf]).expect("merged");
        assert_eq!(merged.sustained.rate, 1_000);
    }

    #[test]
    fn windows_are_compared_per_second() {
        // 10 000/minute is 166.7/s, so the descendant's 100/second is the
        // stricter of the two and survives the merge.
        let root = limit(10_000, RateWindow::Minute, SharingMode::Enforce);
        let leaf = limit(100, RateWindow::Second, SharingMode::Private);
        let merged = merge_rate_limit(&[&root, &leaf]).expect("merged");
        assert_eq!(merged.sustained.rate, 100);
        assert_eq!(merged.sustained.window, RateWindow::Second);

        // Flip the units and the ancestor becomes the stricter layer.
        let root = limit(60, RateWindow::Minute, SharingMode::Enforce);
        let leaf = limit(100, RateWindow::Second, SharingMode::Private);
        let merged = merge_rate_limit(&[&root, &leaf]).expect("merged");
        assert_eq!(merged.sustained.rate, 60);
        assert_eq!(merged.sustained.window, RateWindow::Minute);
    }

    #[test]
    fn inherited_auth_is_overridable_but_enforced_auth_is_not() {
        let inherited = auth(Some("cred://partner"), SharingMode::Inherit);
        let own = auth(Some("cred://own"), SharingMode::Private);
        assert_eq!(
            merge_auth(&[&inherited, &own])
                .expect("merged")
                .plugin_ref
                .as_deref(),
            Some("cred://own")
        );

        let enforced = auth(Some("cred://partner"), SharingMode::Enforce);
        assert_eq!(
            merge_auth(&[&enforced, &own])
                .expect("merged")
                .plugin_ref
                .as_deref(),
            Some("cred://partner")
        );
    }

    #[test]
    fn descendant_without_auth_inherits_visible_ancestor() {
        let inherited = auth(Some("cred://partner"), SharingMode::Inherit);
        let empty = auth(None, SharingMode::Private);
        assert_eq!(
            merge_auth(&[&inherited, &empty])
                .expect("merged")
                .plugin_ref
                .as_deref(),
            Some("cred://partner")
        );

        // A `private` ancestor is invisible, so a descendant that declares
        // no auth of its own ends up with none at all.
        let private = auth(Some("cred://partner"), SharingMode::Private);
        assert!(merge_auth(&[&private, &empty]).is_none());
    }

    #[test]
    fn plugin_chains_concatenate_ancestor_first() {
        let ancestor = PluginsConfig {
            sharing: SharingMode::Inherit,
            items: vec![PluginBinding::named("u1"), PluginBinding::named("u2")],
        };
        let own = PluginsConfig {
            sharing: SharingMode::Private,
            items: vec![PluginBinding::named("r1")],
        };
        let merged = merge_plugins(&[&ancestor, &own]);
        let refs: Vec<&str> = merged.items.iter().map(|b| b.plugin_ref.as_str()).collect();
        assert_eq!(refs, vec!["u1", "u2", "r1"]);
    }

    #[test]
    fn enforced_ancestor_plugins_cannot_be_dropped() {
        let ancestor = PluginsConfig {
            sharing: SharingMode::Enforce,
            items: vec![PluginBinding::named("guard")],
        };
        let own = PluginsConfig::default();
        assert_eq!(merge_plugins(&[&ancestor, &own]).items.len(), 1);
    }

    #[test]
    fn cors_origins_union_under_inherit() {
        let parent = CorsConfig {
            sharing: SharingMode::Inherit,
            enabled: true,
            allowed_origins: vec!["https://app.example.com".to_owned()],
            ..CorsConfig::default()
        };
        let child = CorsConfig {
            enabled: true,
            allowed_origins: vec!["https://admin.example.com".to_owned()],
            ..CorsConfig::default()
        };
        let merged = merge_cors(&[&parent, &child]).expect("merged");
        assert_eq!(merged.allowed_origins.len(), 2);
    }

    #[test]
    fn cors_enforce_pins_the_ancestor_policy() {
        let parent = CorsConfig {
            sharing: SharingMode::Enforce,
            enabled: true,
            allowed_origins: vec!["https://app.example.com".to_owned()],
            ..CorsConfig::default()
        };
        let child = CorsConfig {
            enabled: true,
            allowed_origins: vec!["https://evil.example.com".to_owned()],
            ..CorsConfig::default()
        };
        let merged = merge_cors(&[&parent, &child]).expect("merged");
        assert_eq!(merged.allowed_origins, vec!["https://app.example.com"]);
    }

    #[test]
    fn tags_are_add_only() {
        let root = vec!["llm".to_owned()];
        let leaf = vec!["openai".to_owned()];
        assert_eq!(merge_tags(&[&root, &leaf]), vec!["llm", "openai"]);
    }

    #[test]
    fn ancestor_disable_wins() {
        assert!(!effective_enabled(&[false, true]));
        assert!(effective_enabled(&[true, true]));
    }

    #[test]
    fn visibility_helper_matches_sharing_semantics() {
        assert!(!visible_to_descendant(SharingMode::Private));
        assert!(visible_to_descendant(SharingMode::Inherit));
        assert!(visible_to_descendant(SharingMode::Enforce));
    }
}
