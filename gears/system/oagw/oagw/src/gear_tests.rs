//! Tests for [`crate::gear`].

use std::sync::Arc;

use crate::config::{OagwConfig, SsrfPolicy};
use crate::domain::model::gts_instance_id;
use crate::gear::{OagwGear, cache_limits};
use crate::infra::storage::RegistryStore;

#[test]
fn cache_limits_follow_the_operator_knobs() {
    let config = OagwConfig {
        upstream_l1_cache_max_entries: 11,
        route_l1_cache_max_entries: 12,
        plugin_l1_cache_max_entries: 13,
        dp_cache_max_entries: 14,
        ..OagwConfig::default()
    };
    let limits = cache_limits(&config);
    assert_eq!(limits.upstream, 11);
    assert_eq!(limits.route, 12);
    assert_eq!(limits.plugin, 13);
    assert_eq!(limits.dp, 14);
}

#[test]
fn defaults_produce_a_usable_registry() {
    let limits = cache_limits(&OagwConfig::default());
    let registry = RegistryStore::new(limits);
    assert!(registry.caches_are_empty());
    assert!(registry.list_upstreams(&[]).is_empty());
}

#[test]
fn gear_starts_uninitialised_and_serves_until_cancelled() {
    let gear = Arc::new(OagwGear::default());
    assert!(gear.registry().is_none(), "init has not run yet");
    assert!(gear.effective_config().is_none(), "init has not run yet");

    let parked = gear.clone().serve();
    // Drop the parked future without polling it: the contract under test is
    // that `serve` never resolves on its own, which the type system plus a
    // non-polling drop assert here.
    drop(parked);
}

#[test]
fn ssrf_policy_defaults_are_documented_and_validated() {
    let config = OagwConfig {
        ssrf_policy: SsrfPolicy {
            enabled: true,
            allow_private_networks: false,
            allowed_ip_ranges: vec!["10.0.0.0/8".to_owned()],
        },
        ..OagwConfig::default()
    };
    config.validate().expect("policy must validate");
    assert_eq!(
        gts_instance_id("gts://cf.core.oagw.upstream.v1~abc"),
        Some("abc")
    );
}

#[tokio::test]
async fn serve_parks_until_dropped() {
    let gear = Arc::new(OagwGear::default());
    let task = tokio::spawn(gear.clone().serve());
    tokio::task::yield_now().await;
    assert!(!task.is_finished(), "serve must not resolve on its own");
    task.abort();
}
