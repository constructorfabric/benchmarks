//! Configuration layering (T033): upstream base < route override < tenant
//! ancestor, per field, with `SharingMode` semantics.

use std::collections::BTreeMap;

use crate::domain::dto::{Cors, PluginSet, RateWindow, SharingMode, Sustained};
use crate::domain::layering::{
    EffectiveConfig, fold_route, is_overridable, merge_cors, merge_headers, merge_ancestor_enforced,
    merge_plugins, union_tags, apply_rate_limit,
};
use crate::domain::dto::{HeaderRules, RateLimit, Route, Upstream};

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
fn an_ancestor_enforced_limit_lowers_the_effective_value() {
    let mut config = EffectiveConfig::default();
    config.rate_limit = Some(crate::domain::ratelimit::EffectiveRateLimit::from_config(&limit(100, None)));
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
    let upstream_set = PluginSet { sharing: None, items: vec!["u1".into(), "u2".into()], configs: BTreeMap::new() };
    let route_set = PluginSet { sharing: None, items: vec!["u2".into(), "r1".into()], configs: BTreeMap::new() };
    assert_eq!(
        merge_plugins(Some(&upstream_set), Some(&route_set)),
        vec!["u1", "u2", "r1"]
    );
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
    let ancestor = Cors {
        sharing: Some(SharingMode::Enforce),
        enabled: true,
        ..Cors::default()
    };
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
