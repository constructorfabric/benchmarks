//! Token-bucket arithmetic (T043): capacity, refill, cost, scope keying and
//! the `min()` fold across layers.

use crate::domain::dto::{Burst, RateScope, RateWindow, SharingMode, Sustained};
use crate::domain::ratelimit::{EffectiveRateLimit, TokenBucket, scope_value};
use crate::domain::dto::RateLimit;

fn config(rate: u64, window: RateWindow) -> RateLimit {
    RateLimit {
        sustained: Sustained { rate, window },
        ..RateLimit::default()
    }
}

#[test]
fn burst_is_limited_by_capacity() {
    let mut b = TokenBucket::new(3, 0.0);
    assert_eq!(b.try_acquire(1), Some(2));
    assert_eq!(b.try_acquire(1), Some(1));
    assert_eq!(b.try_acquire(1), Some(0));
    assert_eq!(b.try_acquire(1), None, "capacity is exhausted");
}

#[test]
fn capacity_defaults_to_the_sustained_rate() {
    let cfg = config(10, RateWindow::Second);
    assert_eq!(cfg.capacity(), 10);
    assert_eq!(cfg.refill_rate(), 10.0);
}

#[test]
fn burst_capacity_overrides_the_sustained_rate() {
    let cfg = RateLimit {
        sustained: Sustained { rate: 10, window: RateWindow::Second },
        burst: Some(Burst { capacity: 25 }),
        ..RateLimit::default()
    };
    assert_eq!(cfg.capacity(), 25);
    assert_eq!(cfg.refill_rate(), 10.0);
}

#[test]
fn cost_multiplies_consumption() {
    let mut b = TokenBucket::new(10, 0.0);
    assert_eq!(b.try_acquire(5), Some(5));
    assert_eq!(b.try_acquire(5), Some(0));
    assert_eq!(b.try_acquire(1), None);
}

#[test]
fn the_bucket_never_refills_beyond_capacity() {
    let mut b = TokenBucket::new(2, 1000.0);
    std::thread::sleep(std::time::Duration::from_millis(5));
    b.refill();
    assert_eq!(b.tokens, 2.0, "the bucket is capped at capacity");
}

#[test]
fn a_zero_refill_rate_never_refills() {
    let mut b = TokenBucket::new(1, 0.0);
    b.try_acquire(1);
    std::thread::sleep(std::time::Duration::from_millis(2));
    b.refill();
    assert_eq!(b.try_acquire(1), None);
}

#[test]
fn a_depleted_bucket_reports_the_wait() {
    let mut b = TokenBucket::new(2, 1.0);
    b.try_acquire(2);
    assert!(b.retry_after(1) >= 1, "at least a second to recover a token");
}

#[test]
fn the_usage_ratio_reflects_consumption() {
    let mut b = TokenBucket::new(4, 0.0);
    assert_eq!(b.usage_ratio(), 0.0);
    b.try_acquire(4);
    assert_eq!(b.usage_ratio(), 1.0);
}

#[test]
fn scope_values_are_isolated() {
    let tenant = "11111111-1111-1111-1111-111111111111";
    let user = "22222222-2222-2222-2222-222222222222";
    assert_eq!(scope_value(RateScope::Global, tenant, user, "10.0.0.1", "/v1"), "global");
    assert_eq!(scope_value(RateScope::Tenant, tenant, user, "10.0.0.1", "/v1"), tenant);
    assert_eq!(scope_value(RateScope::User, tenant, user, "10.0.0.1", "/v1"), user);
    assert_eq!(scope_value(RateScope::Ip, tenant, user, "10.0.0.1", "/v1"), "10.0.0.1");
    assert_eq!(scope_value(RateScope::Route, tenant, user, "10.0.0.1", "/v1"), "/v1");
}

#[test]
fn the_effective_limit_is_the_most_restrictive_fold() {
    let lenient = EffectiveRateLimit {
        capacity: 100,
        refill_rate: 100.0,
        cost: 1,
        scope: RateScope::Tenant,
        ancestor_enforced: false,
        response_headers: true,
    };
    let strict = EffectiveRateLimit {
        capacity: 5,
        refill_rate: 2.0,
        cost: 3,
        scope: RateScope::Tenant,
        ancestor_enforced: true,
        response_headers: true,
    };
    let merged = EffectiveRateLimit::min(&lenient, &strict);
    assert_eq!(merged.capacity, 5);
    assert_eq!(merged.refill_rate, 2.0);
    assert_eq!(merged.cost, 3);
    assert!(merged.ancestor_enforced);
}

#[test]
fn a_non_enforced_layer_does_not_mark_the_fold() {
    let base = EffectiveRateLimit {
        capacity: 100,
        refill_rate: 100.0,
        cost: 1,
        scope: RateScope::Tenant,
        ancestor_enforced: false,
        response_headers: true,
    };
    let other = EffectiveRateLimit { ancestor_enforced: false, ..base.clone() };
    assert!(!EffectiveRateLimit::min(&base, &other).ancestor_enforced);
}

#[test]
fn sharing_gates_descendant_overrides() {
    assert!(!crate::domain::ratelimit::sharing_allows_override(Some(SharingMode::Enforce)));
    assert!(crate::domain::ratelimit::sharing_allows_override(Some(SharingMode::Inherit)));
}
