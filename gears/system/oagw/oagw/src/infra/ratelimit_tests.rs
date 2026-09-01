//! Unit tests for [`super::ratelimit`].

#![allow(clippy::unwrap_used, clippy::expect_used)]

use uuid::Uuid;

use super::{LimiterKey, LimiterRegistry};
use crate::domain::merge::merge_upstream_chain;
use crate::domain::models::{
    Protocol,
    BurstCapacity, RateLimitAlgorithm, RateLimitConfig, RateLimitScope, RateLimitStrategy,
    SharingMode, SustainedRate, Upstream,
};

fn config(rate: u64, window_secs: u64, capacity: u64) -> RateLimitConfig {
    RateLimitConfig {
        sharing: SharingMode::Private,
        algorithm: RateLimitAlgorithm::TokenBucket,
        sustained: SustainedRate {
            rate,
            window: window_for(window_secs),
        },
        burst: Some(BurstCapacity { capacity }),
        scope: RateLimitScope::Tenant,
        strategy: RateLimitStrategy::Reject,
        cost: 1,
        response_headers: true,
    }
}

fn window_for(seconds: u64) -> crate::domain::models::RateWindow {
    match seconds {
        1 => crate::domain::models::RateWindow::Second,
        60 => crate::domain::models::RateWindow::Minute,
        _ => crate::domain::models::RateWindow::Hour,
    }
}

fn upstream() -> Upstream {
    Upstream {
        id: Uuid::new_v4(),
        tenant_id: Uuid::new_v4(),
        alias: "api.example.com".to_owned(),
        enabled: true,
        protocol: Protocol::Http,
        server: crate::domain::models::ServerConfig {
            endpoints: Vec::new(),
        },
        auth: None,
        headers: None,
        plugins: None,
        rate_limit: None,
        cors: None,
        tags: Vec::new(),
        created_at: 0,
        updated_at: 0,
    }
}

#[test]
fn a_full_bucket_admits_requests_until_it_is_drained() {
    let registry = LimiterRegistry::new();
    let config = config(1, 1, 2);
    let key = LimiterKey::new(&config, "tenant");

    assert!(registry.check(&config, &key, 1_000).allowed);
    assert!(registry.check(&config, &key, 1_000).allowed);
    let third = registry.check(&config, &key, 1_000);
    assert!(!third.allowed);
    assert_eq!(third.retry_after_seconds, 1);
}

#[test]
fn a_token_bucket_refills_over_time() {
    let registry = LimiterRegistry::new();
    let config = config(1, 1, 1);
    let key = LimiterKey::new(&config, "tenant");

    assert!(registry.check(&config, &key, 1_000).allowed);
    assert!(!registry.check(&config, &key, 1_500).allowed);
    // One second later the token is back.
    assert!(registry.check(&config, &key, 2_100).allowed);
}

#[test]
fn different_scope_keys_have_independent_buckets() {
    let registry = LimiterRegistry::new();
    let config = config(1, 1, 1);
    let tenant_a = LimiterKey::new(&config, "tenant-a");
    let tenant_b = LimiterKey::new(&config, "tenant-b");

    assert!(registry.check(&config, &tenant_a, 1_000).allowed);
    assert!(registry.check(&config, &tenant_b, 1_000).allowed);
    assert!(!registry.check(&config, &tenant_a, 1_000).allowed);
}

#[test]
fn a_config_change_starts_a_fresh_bucket() {
    let registry = LimiterRegistry::new();
    let small = config(1, 1, 1);
    let larger = config(5, 1, 5);
    let key = LimiterKey::new(&small, "tenant");

    assert!(registry.check(&small, &key, 1_000).allowed);
    // The larger configuration is a different limiter, not the drained one.
    assert!(registry.check(&larger, &LimiterKey::new(&larger, "tenant"), 1_000).allowed);
    assert!(!registry.check(&small, &key, 1_000).allowed);
}

#[test]
fn the_sliding_window_counts_requests_over_its_window() {
    let registry = LimiterRegistry::new();
    let mut config = config(2, 60, 2);
    config.algorithm = RateLimitAlgorithm::SlidingWindow;
    // Without a burst block the window limit is the sustained rate.
    config.burst = None;
    let key = LimiterKey::new(&config, "tenant");

    assert!(registry.check(&config, &key, 1_000).allowed);
    assert!(registry.check(&config, &key, 2_000).allowed);
    assert!(!registry.check(&config, &key, 3_000).allowed);
    // Past the window the earliest sample expires.
    assert!(registry.check(&config, &key, 61_500).allowed);
}

#[test]
fn the_merged_chain_rate_limit_drives_the_limiter_key() {
    let mut upstream = upstream();
    upstream.rate_limit = Some(config(10, 1, 10));
    let merged = merge_upstream_chain(&[&upstream]);
    let config = merged.rate_limit.expect("merged rate limit");
    let key = LimiterKey::new(&config, "tenant");

    assert_eq!(key.capacity, 10);
    assert_eq!(key.refill_per_second, 10);
}
