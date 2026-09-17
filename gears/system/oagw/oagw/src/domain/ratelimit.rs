//! Rate-limit decisions and shared primitives.
//!
//! The decision enum is the contract between the (domain-)resolved rate
//! limit and the proxy pipeline; the enforcement implementation lives in
//! `infra::ratelimit` so the domain stays storage-agnostic.

use crate::domain::models::ResolvedRateLimit;

/// Outcome of a rate-limit check for a single request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RateLimitDecision {
    /// Request admitted; `remaining` tokens left in the bucket.
    Allow { remaining: u64 },
    /// Request rejected; caller should surface `Retry-After`.
    Deny { retry_after_seconds: u64 },
}

/// Merge two rate-limit configs with min semantics (DOCS §5.1):
/// `effective = min(own, ancestor_enforced)`. Both the sustained rate and
/// the burst capacity are min'd across the tenant chain. Callers pass an
/// iterator over the resolved ancestor+own configs (already map-ordered
/// tenant chain) and get the strictest combination.
///
/// Returns `None` when no rate-limit configuration applies.
#[must_use]
pub fn merge_effective_rate_limits(
    configs: impl IntoIterator<Item = ResolvedRateLimit>,
) -> Option<ResolvedRateLimit> {
    let mut iter = configs.into_iter();
    let first = iter.next()?;
    let mut acc = first;
    for c in iter {
        acc.rate = acc.rate.min(c.rate);
        acc.capacity = acc.capacity.min(c.capacity);
        // cost: the strictest cost is the largest consumption.
        acc.cost = acc.cost.max(c.cost);
        // window: win by the shortest — a per-second limit is stricter
        // than a per-minute equal-rate limit, so surface the short side.
        acc.window_secs = acc.window_secs.min(c.window_secs);
    }
    Some(acc)
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    fn rl(rate: u32, window_secs: u64, capacity: u32, cost: u32) -> ResolvedRateLimit {
        ResolvedRateLimit {
            rate,
            window_secs,
            capacity,
            cost,
        }
    }

    #[test]
    fn min_semantics_across_chain() {
        // system:10000/min partner:5000/min tenant:1000/min → 1000/min,
        // burst min(1000,500,100) = 100 (DOCS §5.1 Example 1).
        let merged = merge_effective_rate_limits([
            rl(10_000, 60, 1000, 1),
            rl(5_000, 60, 500, 1),
            rl(1_000, 60, 100, 1),
        ])
        .expect("some config");
        assert_eq!(merged.rate, 1_000);
        assert_eq!(merged.capacity, 100);
    }

    #[test]
    fn cost_takes_strictest() {
        let merged = merge_effective_rate_limits([
            rl(100, 60, 100, 1),
            rl(50, 60, 50, 10),
        ])
        .expect("some config");
        assert_eq!(merged.rate, 50);
        assert_eq!(merged.cost, 10);
    }

    #[test]
    fn empty_chain_yields_none() {
        assert_eq!(merge_effective_rate_limits([]), None);
    }
}
