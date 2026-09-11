//! The unit tests of the Data Plane L1 hot-configuration cache
//! (`cpt-cf-oagw-dod-observability-and-state-dp-cache`,
//! `cpt-cf-oagw-dod-observability-and-state-dp-cache-flush`).

use std::collections::BTreeMap;

use uuid::Uuid;

use super::*;
use crate::infra::cp_cache::{CacheKey, ConfigGenerations};

fn upstream_record(alias: &str) -> UpstreamRecord {
    UpstreamRecord {
        upstream: crate::test_support::upstream(Uuid::nil(), alias),
        plugin_bindings: Vec::new(),
    }
}

/// The documented three families are the only key shapes the cache mints.
#[test]
fn the_documented_keys_are_the_three_families() {
    let tenant = Uuid::new_v4();
    let upstream = Uuid::new_v4();
    let plugin = Uuid::new_v4();
    assert_eq!(
        DpHotConfig::upstream_key(tenant, "api.vendor.com"),
        format!("upstream:{tenant}:api.vendor.com")
    );
    assert_eq!(
        DpHotConfig::route_key(upstream, "GET", "/v1"),
        format!("route:{upstream}:GET:/v1")
    );
    assert_eq!(DpHotConfig::plugin_key(plugin), format!("plugin:{plugin}"));
}

/// A populated entry is served back without the store, and an unpopulated one
/// is a miss that inserts nothing.
#[test]
fn a_populated_upstream_entry_is_served_back() {
    let cache = DpHotConfig::new(ConfigGenerations::default());
    let tenant = Uuid::new_v4();
    assert!(cache.get_upstream(tenant, "api.vendor.com").is_none(), "lazy population");
    let observed = cache.observe(&[DpHotConfig::upstream_key(tenant, "api.vendor.com")]);
    cache.put(
        DpHotConfig::upstream_key(tenant, "api.vendor.com"),
        DpValue::Upstream(Arc::new(upstream_record("api.vendor.com"))),
        observed,
    );
    assert_eq!(cache.len(), 1);
    assert_eq!(
        cache.get_upstream(tenant, "api.vendor.com").expect("the entry").upstream.alias,
        "api.vendor.com"
    );
    assert_eq!(cache.get_upstream(tenant, "other.vendor.com"), None);
}

/// An insert that raced a write is refused, so no pre-write value is served.
#[test]
fn a_population_racing_a_write_is_refused() {
    let generations = ConfigGenerations::default();
    let cache = DpHotConfig::new(generations.clone());
    let tenant = Uuid::new_v4();
    let key = DpHotConfig::upstream_key(tenant, "api.vendor.com");
    let observed = cache.observe(&[key.clone()]);
    generations.bump(&key);
    cache.put(key.clone(), DpValue::Upstream(Arc::new(upstream_record("stale"))), observed);
    assert!(cache.is_empty(), "a pre-write value is never inserted");
}

/// A dependency whose generation moved is not served, and the stale entry is
/// dropped rather than kept.
#[test]
fn a_moved_dependency_generation_is_a_miss() {
    let generations = ConfigGenerations::default();
    let cache = DpHotConfig::new(generations.clone());
    let calling = Uuid::new_v4();
    let ancestor = Uuid::new_v4();
    let own = DpHotConfig::upstream_key(calling, "api.vendor.com");
    let walked = DpHotConfig::upstream_key(ancestor, "api.vendor.com");
    let mut observed = BTreeMap::from([(own.clone(), generations.generation(&own))]);
    observed.insert(walked.clone(), generations.generation(&walked));
    cache.put(
        own.clone(),
        DpValue::Upstream(Arc::new(upstream_record("api.vendor.com"))),
        observed,
    );
    assert!(cache.get_upstream(calling, "api.vendor.com").is_some());

    // A descendant tenant writes its own alias: the entry that resolved through
    // the ancestor record is still live, because the ancestor did not move.
    generations.bump(&DpHotConfig::upstream_key(calling, "other.vendor.com"));
    assert!(cache.get_upstream(calling, "api.vendor.com").is_some());

    // The ancestor record the walk resolved through moves: the entry is gone.
    generations.bump(&walked);
    assert!(cache.get_upstream(calling, "api.vendor.com").is_none(), "stale entry dropped");
    assert!(cache.is_empty());
}

/// The flush removes the affected entry and every entry that depends on it.
#[test]
fn a_write_flushes_the_dependent_entries() {
    let cache = DpHotConfig::new(ConfigGenerations::default());
    let calling = Uuid::new_v4();
    let ancestor = Uuid::new_v4();
    let other = Uuid::new_v4();
    let own = DpHotConfig::upstream_key(calling, "api.vendor.com");
    let mut observed = BTreeMap::new();
    observed.insert(own.clone(), 0);
    observed.insert(DpHotConfig::upstream_key(ancestor, "api.vendor.com"), 0);
    cache.put(own.clone(), DpValue::Upstream(Arc::new(upstream_record("api.vendor.com"))), observed);

    let unaffected = DpHotConfig::upstream_key(other, "api.vendor.com");
    cache.put(
        unaffected.clone(),
        DpValue::Upstream(Arc::new(upstream_record("api.vendor.com"))),
        BTreeMap::new(),
    );

    // The write of the ancestor record flushes both its own key and the
    // descendant entry that resolved through it.
    cache.flush(&[CacheKey::Upstream {
        owner_tenant_id: ancestor,
        alias: "api.vendor.com".to_owned(),
    }]);
    assert!(cache.get_upstream(calling, "api.vendor.com").is_none(), "the dependent entry");
    assert!(
        cache.get_upstream(other, "api.vendor.com").is_some(),
        "the unrelated entry stays"
    );
}

/// A write whose affected key set cannot be derived clears the whole cache.
#[test]
fn an_underivable_key_set_clears_the_whole_cache() {
    let cache = DpHotConfig::new(ConfigGenerations::default());
    let tenant = Uuid::new_v4();
    let key = DpHotConfig::upstream_key(tenant, "api.vendor.com");
    cache.put(
        key,
        DpValue::Upstream(Arc::new(upstream_record("api.vendor.com"))),
        BTreeMap::new(),
    );
    cache.flush_all();
    assert!(cache.is_empty());
}

/// The walk's dependency set names the calling alias and every level the walk
/// found a record at.
#[test]
fn the_walk_dependency_set_names_every_level() {
    let calling = Uuid::new_v4();
    let ancestor = Uuid::new_v4();
    let levels = vec![
        crate::infra::proxy::alias_resolver::AliasLevel {
            tenant_id: calling,
            distance: 0,
            record: None,
        },
        crate::infra::proxy::alias_resolver::AliasLevel {
            tenant_id: ancestor,
            distance: 1,
            record: Some(upstream_record("api.vendor.com")),
        },
    ];
    let dependencies = upstream_dependencies(calling, "api.vendor.com", &levels);
    assert_eq!(dependencies.len(), 2, "the own key and the level the walk found");
    assert!(dependencies.contains(&DpHotConfig::upstream_key(calling, "api.vendor.com")));
    assert!(dependencies.contains(&DpHotConfig::upstream_key(ancestor, "api.vendor.com")));
}

/// The capacity is the fixed 1,000-entry constant, and the LRU evicts the
/// least recently used entry at capacity.
#[test]
fn the_dp_capacity_is_the_fixed_constant() {
    assert_eq!(DP_L1_CAPACITY, 1_000);
    let cache = DpHotConfig::new(ConfigGenerations::default());
    let keys: Vec<String> =
        (0..=DP_L1_CAPACITY).map(|index| format!("upstream:{index}:a")).collect();
    for key in keys.iter().take(DP_L1_CAPACITY) {
        cache.put(
            key.clone(),
            DpValue::Upstream(Arc::new(upstream_record("api.vendor.com"))),
            BTreeMap::new(),
        );
    }
    assert_eq!(cache.len(), DP_L1_CAPACITY);
    // A read makes the oldest entry the most recently used, so the next
    // insertion evicts a different one.
    assert!(cache.get(&keys[0]).is_some());
    cache.put(
        keys[DP_L1_CAPACITY].clone(),
        DpValue::Upstream(Arc::new(upstream_record("api.vendor.com"))),
        BTreeMap::new(),
    );
    assert_eq!(cache.len(), DP_L1_CAPACITY);
    assert!(cache.get(&keys[0]).is_some(), "the refreshed entry survives");
    assert!(cache.get(&keys[1]).is_none(), "the least recently used entry is evicted");
}

/// No negative caching: a miss inserts nothing.
#[test]
fn a_miss_inserts_nothing() {
    let cache = DpHotConfig::new(ConfigGenerations::default());
    let tenant = Uuid::new_v4();
    assert!(cache.get_upstream(tenant, "missing.vendor.com").is_none());
    assert!(cache.is_empty());
    assert!(cache.get_route(Uuid::new_v4(), "GET", "/v1").is_none());
    assert!(cache.get_plugin(Uuid::nil()).is_none());
}
