use std::time::{Duration, Instant};

use super::{
    Admission, QUEUE_WAIT, RateLimiter, ScopeKey, TokenBucket, rate_limit_headers, scope_key,
};
use crate::domain::model::{
    Burst, RateLimitConfig, RateLimitScope, RateLimitStrategy, RateWindow, SustainedRate,
};

/// A limit of `rate` tokens per second with an optional burst capacity.
fn limit(rate: u32, burst: Option<u32>, strategy: RateLimitStrategy) -> RateLimitConfig {
    RateLimitConfig {
        sharing: crate::domain::model::Sharing::Inherit,
        algorithm: crate::domain::model::RateLimitAlgorithm::TokenBucket,
        sustained: SustainedRate {
            rate,
            window: RateWindow::Second,
        },
        burst: burst.map(|capacity| Burst { capacity }),
        scope: RateLimitScope::Tenant,
        strategy,
        cost: 1,
        response_headers: true,
    }
}

fn key(name: &str) -> ScopeKey {
    ScopeKey {
        scope: "tenant".to_owned(),
        identity: name.to_owned(),
        resource: "upstream".to_owned(),
    }
}

#[test]
fn a_fresh_bucket_starts_full() {
    let config = limit(10, Some(10), RateLimitStrategy::Reject);
    let bucket = TokenBucket::new(&config, Instant::now());
    assert_eq!(bucket.remaining(), 10);
}

#[test]
fn burst_capacity_defaults_to_the_sustained_rate() {
    let config = limit(5, None, RateLimitStrategy::Reject);
    assert_eq!(config.capacity(), 5);
    let config = limit(5, Some(20), RateLimitStrategy::Reject);
    assert_eq!(config.capacity(), 20);
}

#[test]
fn tokens_refill_over_time_and_never_exceed_capacity() {
    let config = limit(10, Some(10), RateLimitStrategy::Reject);
    let start = Instant::now();
    let mut bucket = TokenBucket::new(&config, start);
    assert_eq!(bucket.remaining(), 10);
    // Nothing may exceed the capacity, even after a long idle period.
    bucket.refill(start + Duration::from_mins(10));
    assert_eq!(bucket.remaining(), 10);

    // Drain the bucket, then let it refill at the sustained rate. The clock
    // only ever moves forward: a bucket refilled in the future stays there.
    let idle = start + Duration::from_mins(10);
    let acquired = (0..10).all(|_| bucket.try_acquire(1, idle + Duration::from_millis(1)));
    assert!(acquired, "a full bucket must serve ten requests");
    assert_eq!(bucket.remaining(), 0);
    bucket.refill(idle + Duration::from_millis(1500));
    assert_eq!(bucket.remaining(), 10);
}

#[test]
fn an_emptied_bucket_refuses_until_it_refills() {
    let config = limit(1, Some(1), RateLimitStrategy::Reject);
    let start = Instant::now();
    let mut bucket = TokenBucket::new(&config, start);
    assert!(bucket.try_acquire(1, start));
    assert!(!bucket.try_acquire(1, start + Duration::from_millis(200)));
    assert!(bucket.try_acquire(1, start + Duration::from_secs(1)));
}

#[test]
fn refill_rate_follows_the_configured_window() {
    let per_minute = RateLimitConfig {
        sharing: crate::domain::model::Sharing::Inherit,
        algorithm: crate::domain::model::RateLimitAlgorithm::TokenBucket,
        sustained: SustainedRate {
            rate: 60,
            window: RateWindow::Minute,
        },
        burst: Some(Burst { capacity: 60 }),
        scope: RateLimitScope::Tenant,
        strategy: RateLimitStrategy::Reject,
        cost: 1,
        response_headers: true,
    };
    let start = Instant::now();
    let mut bucket = TokenBucket::new(&per_minute, start);
    for _ in 0..60 {
        assert!(bucket.try_acquire(1, start));
    }
    assert_eq!(bucket.remaining(), 0);
    // Sixty tokens per minute is one token per second.
    bucket.refill(start + Duration::from_secs(1));
    assert_eq!(bucket.remaining(), 1);
}

#[tokio::test]
async fn reject_strategy_refuses_and_reports_headers() {
    let limiter = RateLimiter::new();
    let config = limit(1, Some(1), RateLimitStrategy::Reject);
    let scope = key("reject");
    let now = Instant::now();
    let first = limiter.check(&config, &scope, now).await;
    assert!(first.allowed);
    let second = limiter
        .check(&config, &scope, now + Duration::from_millis(10))
        .await;
    assert!(!second.allowed);
    assert!(
        second.retry_after_seconds >= 1,
        "retry guidance is required"
    );

    let headers = rate_limit_headers(&second, &config);
    assert_eq!(
        headers.get("X-RateLimit-Limit").map(String::as_str),
        Some("1")
    );
    assert_eq!(
        headers.get("X-RateLimit-Remaining").map(String::as_str),
        Some("0")
    );
    assert!(headers.contains_key("X-RateLimit-Reset"));
}

#[tokio::test]
async fn degrade_strategy_admits_and_marks_the_request() {
    let limiter = RateLimiter::new();
    let config = limit(1, Some(1), RateLimitStrategy::Degrade);
    let scope = key("degrade");
    let now = Instant::now();
    assert!(limiter.check(&config, &scope, now).await.allowed);
    let degraded = limiter.check(&config, &scope, now).await;
    assert!(degraded.allowed);
    assert!(degraded.degraded);
    assert_eq!(
        rate_limit_headers(&degraded, &config)
            .get("X-OAGW-Rate-Limited")
            .map(String::as_str),
        Some("degraded")
    );
}

#[tokio::test]
async fn queue_strategy_waits_for_a_token() {
    let limiter = RateLimiter::new();
    let config = limit(1, Some(1), RateLimitStrategy::Queue);
    let scope = key("queue");
    let now = Instant::now();
    assert!(limiter.check(&config, &scope, now).await.allowed);
    let queued = limiter.check(&config, &scope, now).await;
    assert!(
        !queued.allowed,
        "a one-per-second bucket cannot refill within 250ms"
    );
    assert!(QUEUE_WAIT <= Duration::from_millis(300));
}

#[tokio::test]
async fn distinct_scopes_never_share_a_bucket() {
    let limiter = RateLimiter::new();
    let config = limit(1, Some(1), RateLimitStrategy::Reject);
    let now = Instant::now();
    assert!(limiter.check(&config, &key("tenant-a"), now).await.allowed);
    assert!(limiter.check(&config, &key("tenant-b"), now).await.allowed);
    assert!(!limiter.check(&config, &key("tenant-a"), now).await.allowed);
}

#[tokio::test]
async fn forgetting_a_resource_drops_its_buckets() {
    let limiter = RateLimiter::new();
    let config = limit(1, Some(1), RateLimitStrategy::Reject);
    let scope = key("forget");
    let now = Instant::now();
    assert!(limiter.check(&config, &scope, now).await.allowed);
    limiter.forget_resource("upstream");
    assert!(limiter.check(&config, &scope, now).await.allowed);
}

#[test]
fn scope_keys_follow_the_configured_scope() {
    let tenant = uuid::Uuid::from_u128(2);
    let subject = uuid::Uuid::from_u128(3);
    let route = uuid::Uuid::from_u128(4);

    let mut config = limit(1, None, RateLimitStrategy::Reject);
    config.scope = RateLimitScope::Global;
    let global = scope_key(&config, "resource", tenant, subject, None, None);
    assert_eq!(global.identity, "global");

    config.scope = RateLimitScope::Tenant;
    assert_eq!(
        scope_key(&config, "resource", tenant, subject, None, None).identity,
        tenant.to_string()
    );

    config.scope = RateLimitScope::User;
    let user = scope_key(&config, "resource", tenant, subject, None, None);
    assert_eq!(user.identity, format!("{tenant}:{subject}"));
    assert_eq!(user.scope, "user");

    config.scope = RateLimitScope::Ip;
    assert_eq!(
        scope_key(&config, "resource", tenant, subject, Some("10.0.0.1"), None).identity,
        "10.0.0.1"
    );
    assert_eq!(
        scope_key(&config, "resource", tenant, subject, None, None).identity,
        "unknown"
    );

    config.scope = RateLimitScope::Route;
    assert_eq!(
        scope_key(&config, "resource", tenant, subject, None, Some(route)).identity,
        route.to_string()
    );
    assert_eq!(
        scope_key(&config, "resource", tenant, subject, None, None).identity,
        "resource"
    );
}

#[test]
fn response_headers_are_opt_out() {
    let admission = Admission {
        allowed: true,
        limit: 5,
        remaining: 4,
        reset_at: 1_700_000_000,
        retry_after_seconds: 0,
        degraded: false,
    };
    let mut config = limit(5, None, RateLimitStrategy::Reject);
    config.response_headers = false;
    let headers = rate_limit_headers(&admission, &config);
    assert!(!headers.contains_key("X-RateLimit-Limit"));
    assert!(!headers.contains_key("X-RateLimit-Reset"));
}
