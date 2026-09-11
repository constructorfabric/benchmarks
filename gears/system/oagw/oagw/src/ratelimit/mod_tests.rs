//! Tests for the token-bucket rate limiter (ADR-0003).

use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::domain::upstream::{
    RateLimit, RateLimitAlgorithm, RateLimitScope, RateWindow,
};
use crate::ratelimit::{is_sliding_window, window_secs, Decision, ManualClock, Rate, RateLimiter};

fn policy(rate: u64, window_secs: u64, burst: Option<u64>) -> RateLimit {
    RateLimit {
        sharing: "tenant".to_owned(),
        algorithm: RateLimitAlgorithm::TokenBucket,
        sustained: crate::domain::upstream::SustainedRate {
            rate,
            window: match window_secs {
                0 | 1 => RateWindow::Second,
                60 => RateWindow::Minute,
                3600 => RateWindow::Hour,
                _ => RateWindow::Day,
            },
        },
        burst: burst.map(|capacity| crate::domain::upstream::BurstConfig { capacity }),
        scope: RateLimitScope::Ip,
        ..RateLimit::default()
    }
}

fn limiter() -> (RateLimiter, Arc<ManualClock>) {
    let clock = Arc::new(ManualClock::new(Instant::now()));
    (RateLimiter::new(clock.clone()), clock)
}

#[test]
fn a_bucket_starts_full_and_drains_by_the_cost() {
    let (limiter, _) = limiter();
    let policy = policy(1, 1, Some(3));
    let key = "ip:10.0.0.1";

    let first = limiter.check(key, &policy);
    assert!(first.allowed());
    assert_eq!(first.remaining, 2);

    let second = limiter.check(key, &policy);
    assert_eq!(second.remaining, 1);

    let third = limiter.check(key, &policy);
    assert_eq!(third.remaining, 0);

    let fourth = limiter.check(key, &policy);
    assert!(!fourth.allowed(), "the bucket is empty");
}

#[test]
fn a_rejected_request_is_not_charged() {
    let (limiter, _) = limiter();
    let policy = policy(1, 1, Some(1));
    let key = "ip:10.0.0.2";

    assert!(limiter.check(key, &policy).allowed());
    let rejected = limiter.check(key, &policy);
    assert!(!rejected.allowed());
    assert_eq!(limiter.peek(key, &policy), 0, "the empty bucket stays empty");
}

#[test]
fn the_bucket_refills_at_the_sustained_rate() {
    let (limiter, clock) = limiter();
    let policy = policy(2, 1, Some(2));
    let key = "ip:10.0.0.3";

    assert!(limiter.check(key, &policy).allowed());
    assert!(limiter.check(key, &policy).allowed());
    assert_eq!(limiter.peek(key, &policy), 0);

    clock.advance(Duration::from_secs(1));
    assert_eq!(limiter.peek(key, &policy), 2, "two tokens per second");
    assert!(limiter.check(key, &policy).allowed());
}

#[test]
fn retry_after_reports_when_the_bucket_can_pay() {
    let (limiter, clock) = limiter();
    let policy = policy(1, 1, Some(1));
    let key = "ip:10.0.0.3";

    assert!(limiter.check(key, &policy).allowed());
    let rejected = limiter.check(key, &policy);
    assert!(!rejected.allowed());
    assert!(rejected.retry_after_secs >= 1);

    clock.advance(Duration::from_secs(2));
    assert!(
        limiter.check(key, &policy).allowed(),
        "the bucket refilled while the caller waited"
    );
}

#[test]
fn a_higher_cost_needs_a_deeper_bucket() {
    let (limiter, _) = limiter();
    let mut policy = policy(1, 1, Some(4));
    policy.cost = 3;
    let key = "ip:10.0.0.4";

    let decision = limiter.check(key, &policy);
    assert!(decision.allowed());
    assert_eq!(decision.remaining, 1);

    assert!(
        !limiter.check(key, &policy).allowed(),
        "one token cannot pay a cost of three"
    );
}

#[test]
fn separate_scopes_keep_separate_buckets() {
    let (limiter, _) = limiter();
    let policy = policy(1, 1, Some(1));
    assert!(limiter.check("ip:a", &policy).allowed());
    assert!(limiter.check("ip:b", &policy).allowed());
    assert!(!limiter.check("ip:a", &policy).allowed());
}

#[test]
fn the_scope_key_names_the_caller() {
    let security = crate::security::SecurityContextHolder::new(
        test_context(),
        Vec::new(),
    );
    let mut policy = policy(1, 1, None);

    policy.scope = RateLimitScope::Global;
    assert_eq!(RateLimiter::key(&policy, &security, "route", "10.0.0.1"), "global");

    policy.scope = RateLimitScope::Tenant;
    let key = RateLimiter::key(&policy, &security, "route", "10.0.0.1");
    assert!(key.starts_with("tenant:"), "{key}");

    policy.scope = RateLimitScope::User;
    let key = RateLimiter::key(&policy, &security, "route", "10.0.0.1");
    assert!(key.starts_with("user:"), "{key}");

    policy.scope = RateLimitScope::Ip;
    assert_eq!(
        RateLimiter::key(&policy, &security, "route", "10.0.0.1"),
        "ip:10.0.0.1"
    );

    policy.scope = RateLimitScope::Route;
    assert_eq!(
        RateLimiter::key(&policy, &security, "route-1", "10.0.0.1"),
        "route:route-1"
    );
}

#[test]
fn the_per_second_rate_is_ceil_divided_over_the_window() {
    let rate = Rate::from_policy(&policy(10, 1, None));
    assert_eq!(rate.per_second, 10);
    assert_eq!(rate.capacity, 10);

    let rate = Rate::from_policy(&policy(1, 60, None));
    assert_eq!(rate.per_second, 1, "a sub-second sustained rate rounds up");
    assert_eq!(rate.capacity, 1);
}

#[test]
fn the_burst_capacity_is_the_bucket_size() {
    let rate = Rate::from_policy(&policy(1, 1, Some(100)));
    assert_eq!(rate.capacity, 100);
}

#[test]
fn a_sub_second_window_is_never_zero() {
    // A window is expressed in whole units, so the smallest window is one second; the
    // scaling guard is that a zero-length window never divides the refill by zero.
    let policy = policy(1, 1, None);
    assert!(Rate::from_policy(&policy).per_second >= 1);
}

#[test]
fn the_window_helpers_report_the_configuration() {
    assert_eq!(window_secs(RateWindow::Second), 1);
    assert_eq!(window_secs(RateWindow::Minute), 60);
    assert_eq!(window_secs(RateWindow::Hour), 3600);
    assert_eq!(window_secs(RateWindow::Day), 86_400);
    assert!(is_sliding_window(RateLimitAlgorithm::SlidingWindow));
    assert!(!is_sliding_window(RateLimitAlgorithm::TokenBucket));
}

#[test]
fn a_zero_retry_after_means_admitted() {
    let decision = Decision {
        remaining: 1,
        retry_after_secs: 0,
    };
    assert!(decision.allowed());
    let decision = Decision {
        remaining: 0,
        retry_after_secs: 3,
    };
    assert!(!decision.allowed());
}

fn test_context() -> toolkit_security::SecurityContext {
    let tenant = uuid::Uuid::new_v4();
    toolkit_security::SecurityContext::builder()
        .subject_id(uuid::Uuid::new_v4())
        .subject_tenant_id(tenant)
        .build()
        .unwrap_or_else(|_| toolkit_security::SecurityContext::anonymous())
}
