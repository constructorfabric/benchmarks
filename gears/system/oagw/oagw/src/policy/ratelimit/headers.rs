//! Rate-limit response-header computation
//! (`cpt-cf-oagw-algo-ratelimit-headers`): `X-RateLimit-Limit`/`-Remaining`/
//! `-Reset` on every response for which a bucket was evaluated, plus
//! `Retry-After` on a `strategy: reject` denial.

use std::time::{Duration, Instant};

use axum::http::{HeaderMap, HeaderName, HeaderValue};

use crate::model::upstream::RateLimitStrategy;
use crate::policy::ratelimit::bucket::ConsumeDecision;

pub(crate) const HEADER_LIMIT: HeaderName = HeaderName::from_static("x-ratelimit-limit");
pub(crate) const HEADER_REMAINING: HeaderName = HeaderName::from_static("x-ratelimit-remaining");
pub(crate) const HEADER_RESET: HeaderName = HeaderName::from_static("x-ratelimit-reset");

/// The header set [`compute_headers`] returns, ready to be inserted onto
/// whichever response the calling strategy ultimately returns
/// (`inst-ratelimit-headers-06`).
#[derive(Debug, Clone, Copy)]
pub(crate) struct RateLimitHeaders {
    pub limit: u32,
    pub remaining: u32,
    pub reset_unix: u64,
    pub retry_after_secs: Option<u64>,
}

impl RateLimitHeaders {
    /// Insert this header set onto `headers`, overwriting any prior value
    /// under the same names.
    pub(crate) fn apply(&self, headers: &mut HeaderMap) {
        headers.insert(HEADER_LIMIT, HeaderValue::from(self.limit));
        headers.insert(HEADER_REMAINING, HeaderValue::from(self.remaining));
        headers.insert(HEADER_RESET, HeaderValue::from(self.reset_unix));
        if let Some(retry_after) = self.retry_after_secs {
            headers.insert(
                axum::http::header::RETRY_AFTER,
                HeaderValue::from(retry_after),
            );
        }
    }
}

/// `cpt-cf-oagw-algo-ratelimit-headers`: derive the header set from a
/// `cpt-cf-oagw-algo-ratelimit-consume` outcome. `now`/`now_unix` are the
/// monotonic and wall-clock readings of the same instant, so `reset_at`
/// (monotonic) can be expressed as a Unix timestamp
/// (`inst-ratelimit-headers-03`) without this module depending on a
/// particular wall-clock source itself.
///
/// `limit_capacity` is the bucket capacity, so `X-RateLimit-Limit` and
/// `X-RateLimit-Remaining` are always on the same scale and `Remaining`
/// can never exceed `Limit` (ADR-0003 pairs `Limit: 100`/`Remaining: 0`).
// @cpt-algo:cpt-cf-oagw-algo-ratelimit-headers:p2
// @cpt-begin:cpt-cf-oagw-algo-ratelimit-headers:p2:inst-ratelimit-headers-01
// @cpt-begin:cpt-cf-oagw-algo-ratelimit-headers:p2:inst-ratelimit-headers-02
// @cpt-begin:cpt-cf-oagw-algo-ratelimit-headers:p2:inst-ratelimit-headers-03
// @cpt-begin:cpt-cf-oagw-algo-ratelimit-headers:p2:inst-ratelimit-headers-04
// @cpt-begin:cpt-cf-oagw-algo-ratelimit-headers:p2:inst-ratelimit-headers-05
// @cpt-begin:cpt-cf-oagw-algo-ratelimit-headers:p2:inst-ratelimit-headers-06
pub(crate) fn compute_headers(
    decision: ConsumeDecision,
    remaining: u32,
    limit_capacity: u32,
    reset_at: Instant,
    now: Instant,
    now_unix: u64,
    strategy: RateLimitStrategy,
) -> RateLimitHeaders {
    let delta = reset_at
        .checked_duration_since(now)
        .unwrap_or(Duration::ZERO);
    let reset_unix = now_unix.saturating_add(round_secs(delta));

    // RF-007: the queue-full rejection uses the same `429` contract as an
    // ordinary `reject` denial, which includes `Retry-After`
    // (`rate-limiting.md`); only `degrade` (which forwards rather than
    // rejecting) omits it.
    let retry_after_secs = (decision == ConsumeDecision::Deny
        && strategy != RateLimitStrategy::Degrade)
        .then(|| ceil_secs(delta));

    RateLimitHeaders {
        limit: limit_capacity,
        remaining,
        reset_unix,
        retry_after_secs,
    }
}

#[allow(
    clippy::cast_possible_truncation,
    reason = "reset delays are bounded to a single sustained.window (schema max 'day'), far below u64"
)]
fn round_secs(delta: Duration) -> u64 {
    delta.as_secs_f64().round() as u64
}

#[allow(
    clippy::cast_possible_truncation,
    reason = "reset delays are bounded to a single sustained.window (schema max 'day'), far below u64"
)]
fn ceil_secs(delta: Duration) -> u64 {
    delta.as_secs_f64().ceil() as u64
}
// @cpt-end:cpt-cf-oagw-algo-ratelimit-headers:p2:inst-ratelimit-headers-06
// @cpt-end:cpt-cf-oagw-algo-ratelimit-headers:p2:inst-ratelimit-headers-05
// @cpt-end:cpt-cf-oagw-algo-ratelimit-headers:p2:inst-ratelimit-headers-04
// @cpt-end:cpt-cf-oagw-algo-ratelimit-headers:p2:inst-ratelimit-headers-03
// @cpt-end:cpt-cf-oagw-algo-ratelimit-headers:p2:inst-ratelimit-headers-02
// @cpt-end:cpt-cf-oagw-algo-ratelimit-headers:p2:inst-ratelimit-headers-01

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allow_carries_the_three_headers_but_no_retry_after() {
        let now = Instant::now();
        let headers = compute_headers(
            ConsumeDecision::Allow,
            9,
            10,
            now,
            now,
            1_700_000_000,
            RateLimitStrategy::Reject,
        );
        assert_eq!(headers.limit, 10);
        assert_eq!(headers.remaining, 9);
        assert_eq!(headers.reset_unix, 1_700_000_000);
        assert!(headers.retry_after_secs.is_none());
    }

    #[test]
    fn reject_denial_adds_retry_after_rounded_up() {
        let now = Instant::now();
        let reset_at = now + Duration::from_millis(2500);
        let headers = compute_headers(
            ConsumeDecision::Deny,
            0,
            10,
            reset_at,
            now,
            1_700_000_000,
            RateLimitStrategy::Reject,
        );
        assert_eq!(headers.retry_after_secs, Some(3));
        assert_eq!(headers.reset_unix, 1_700_000_003);
    }

    /// RF-007: `rate-limiting.md`'s queue-full rejection uses the same `429`
    /// contract as `strategy: reject`, which includes `Retry-After` -- a
    /// `strategy: queue` denial (the bounded-queue-full case) must carry it
    /// too, not just `strategy: reject`.
    #[test]
    fn queue_denial_adds_retry_after_the_same_as_reject() {
        let now = Instant::now();
        let reset_at = now + Duration::from_millis(1500);
        let headers = compute_headers(
            ConsumeDecision::Deny,
            0,
            10,
            reset_at,
            now,
            1_700_000_000,
            RateLimitStrategy::Queue,
        );
        assert_eq!(headers.retry_after_secs, Some(2));
    }

    #[test]
    fn degrade_denial_carries_headers_without_retry_after() {
        let now = Instant::now();
        let reset_at = now + Duration::from_secs(1);
        let headers = compute_headers(
            ConsumeDecision::Deny,
            0,
            10,
            reset_at,
            now,
            1_700_000_000,
            RateLimitStrategy::Degrade,
        );
        assert!(headers.retry_after_secs.is_none());
        assert_eq!(headers.remaining, 0);
    }

    #[test]
    fn apply_inserts_all_documented_header_names() {
        let headers = RateLimitHeaders {
            limit: 5,
            remaining: 5,
            reset_unix: 123,
            retry_after_secs: Some(7),
        };
        let mut map = HeaderMap::new();
        headers.apply(&mut map);
        assert_eq!(map.get(HEADER_LIMIT).unwrap(), "5");
        assert_eq!(map.get(HEADER_REMAINING).unwrap(), "5");
        assert_eq!(map.get(HEADER_RESET).unwrap(), "123");
        assert_eq!(map.get(axum::http::header::RETRY_AFTER).unwrap(), "7");
    }
}
