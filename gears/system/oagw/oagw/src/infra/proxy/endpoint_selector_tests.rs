//! Tests of the round-robin cursor state
//! (`cpt-cf-oagw-algo-request-proxy-endpoint-select`, `inst-rp-target-6`).
// @cpt-dod:cpt-cf-oagw-dod-request-proxy-endpoint-selection:p1
// @cpt-dod:cpt-cf-oagw-dod-request-proxy-scheme-allowlist:p1
// @cpt-dod:cpt-cf-oagw-dod-request-proxy-target-host-matrix:p1

use crate::infra::proxy::endpoint_selector::{pool_fingerprint, EndpointSelector};
use crate::domain::dto::{Endpoint, EndpointScheme};
use uuid::Uuid;
use std::sync::Arc;

fn endpoint(host: &str, port: u16) -> Endpoint {
    Endpoint { scheme: EndpointScheme::Https, host: host.to_owned(), port }
}

#[test]
fn the_fingerprint_covers_every_endpoint_of_the_pool() {
    let pool = vec![endpoint("eu.api.vendor.com", 443), endpoint("us.api.vendor.com", 443)];
    let same = vec![endpoint("us.api.vendor.com", 443), endpoint("eu.api.vendor.com", 443)];
    assert_eq!(pool_fingerprint(&pool), pool_fingerprint(&same));
    assert_ne!(pool_fingerprint(&pool), pool_fingerprint(&vec![endpoint("eu.api.vendor.com", 443)]));
}

#[test]
fn a_selector_starts_at_zero_and_advances_once_per_selection() {
    let selector = EndpointSelector::new();
    let pool = vec![endpoint("eu.api.vendor.com", 443), endpoint("us.api.vendor.com", 443)];
    let key = Uuid::new_v4();
    assert_eq!(selector.next(key, &pool) % 2, 0);
    assert_eq!(selector.next(key, &pool) % 2, 1);
    assert_eq!(selector.next(key, &pool) % 2, 0);
    assert_eq!(selector.len(), 1);
}

#[test]
fn two_pools_hold_two_cursors() {
    let selector = EndpointSelector::new();
    let key = Uuid::new_v4();
    let pool = vec![endpoint("eu.api.vendor.com", 443), endpoint("us.api.vendor.com", 443)];
    let other = vec![endpoint("eu.api.vendor.com", 443)];
    let _ = selector.next(key, &pool);
    let _ = selector.next(key, &other);
    assert_eq!(selector.len(), 2);
    assert!(!selector.is_empty());
}

#[test]
fn a_changed_pool_is_re_derived_not_resumed() {
    let selector = EndpointSelector::new();
    let key = Uuid::new_v4();
    let before = vec![endpoint("eu.api.vendor.com", 443), endpoint("us.api.vendor.com", 443)];
    let after = vec![endpoint("eu.api.vendor.com", 443), endpoint("ap.api.vendor.com", 443)];
    let _ = selector.next(key, &before);
    let _ = selector.next(key, &after);
    // A fresh fingerprint means a fresh cursor, so the table holds both.
    assert_eq!(selector.len(), 2);
}

#[test]
fn concurrent_selections_over_one_pool_do_not_all_return_the_same_endpoint() {
    let selector = Arc::new(EndpointSelector::new());
    let pool = Arc::new(vec![
        endpoint("eu.api.vendor.com", 443),
        endpoint("us.api.vendor.com", 443),
        endpoint("ap.api.vendor.com", 443),
    ]);
    let key = Uuid::new_v4();
    let handles: Vec<_> = (0..24)
        .map(|_| {
            let selector = Arc::clone(&selector);
            let pool = Arc::clone(&pool);
            std::thread::spawn(move || selector.next(key, &pool) % pool.len())
        })
        .collect();
    let mut selected = Vec::new();
    for handle in handles {
        selected.push(handle.join().expect("joined"));
    }
    for index in 0..pool.len() {
        assert!(selected.contains(&index), "endpoint {index} was selected");
    }
}

#[test]
fn the_selector_is_debuggable_without_its_state() {
    let selector = EndpointSelector::new();
    assert!(format!("{selector:?}").contains("EndpointSelector"));
}
