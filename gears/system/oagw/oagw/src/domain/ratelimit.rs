//! Effective rate-limit policy computation (ADR-0003).
//!
//! Rate limits merge across the tenant hierarchy with the rule
//! `effective = min(ancestor_enforced, descendant)`: every `enforce` ancestor
//! and the matched route's limit participate in the `min`, the strictest
//! always wins. A `private` ancestor limit is invisible to descendants, an
//! `inherit` limit is a **default** that applies only when the descendant
//! declares no limit of its own (`PRD.md` §5.5 "Descendant can only be
//! stricter: `effective = min(ancestor.enforced, descendant)`").

use crate::domain::model::{RateLimitConfig, SharingMode};

/// A limit contribution along the tenant chain, with the mode it carried.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LimitContribution<'a> {
    /// Tenant the limit came from (root first).
    pub tenant_id: uuid::Uuid,
    /// The configured limit.
    pub limit: &'a RateLimitConfig,
}

/// Computes the effective rate limit for a request.
///
/// `chain` is ordered root → descendant; `own` is the selected (shadowed)
/// upstream's limit and `route` the matched route's limit.
///
/// * every `enforce` ancestor limit participates in the `min`, as does the
///   route limit — neither can be exceeded;
/// * the **nearest** `inherit` ancestor limit is a default that applies only
///   when the descendant supplies no own limit (an `inherit` ancestor never
///   caps a descendant that configured a limit of its own);
/// * `private` ancestor limits stay invisible.
///
/// Returns `None` when no limit applies.
#[must_use]
pub fn effective_limit<'a>(
    chain: &[LimitContribution<'a>],
    own: Option<&'a RateLimitConfig>,
    route: Option<&'a RateLimitConfig>,
) -> Option<RateLimitConfig> {
    let enforced = chain
        .iter()
        .filter(|c| c.limit.sharing == SharingMode::Enforce)
        .map(|c| c.limit);
    // The nearest `inherit` ancestor is the fallback default: it is only a
    // candidate while the descendant contributes nothing of its own.
    let inherited_default = if own.is_none() {
        chain
            .iter()
            .rev()
            .find(|c| c.limit.sharing == SharingMode::Inherit)
            .map(|c| c.limit)
    } else {
        None
    };

    let candidates: Vec<&RateLimitConfig> = enforced
        .chain(inherited_default)
        .chain(own)
        .chain(route)
        .collect();

    let first = candidates.first()?;

    let sustained_rate = candidates.iter().map(|limit| limit.sustained.rate).min()?;
    let sustained_window = candidates
        .iter()
        .min_by_key(|limit| limit.sustained.window.as_secs())
        .map_or(first.sustained.window, |limit| limit.sustained.window);
    let burst_capacity = candidates
        .iter()
        .map(|limit| limit.capacity())
        .min()
        .unwrap_or(first.capacity());
    let cost = candidates.iter().map(|limit| limit.cost).max();

    let mut effective = (*first).clone();
    effective.sustained.rate = sustained_rate;
    effective.sustained.window = sustained_window;
    effective.burst = Some(crate::domain::model::BurstCapacity {
        capacity: burst_capacity,
    });
    if let Some(cost) = cost {
        effective.cost = cost;
    }
    // Response headers and scope are taken from the winning (strictest) limit.
    effective.response_headers = first.response_headers;
    Some(effective)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use crate::domain::model::{BurstCapacity, RateWindow, SustainedRate};

    fn limit(
        mode: SharingMode,
        rate: u32,
        window: RateWindow,
        burst: Option<u32>,
    ) -> RateLimitConfig {
        RateLimitConfig {
            sharing: mode,
            algorithm: crate::domain::model::RateAlgorithm::TokenBucket,
            sustained: SustainedRate { rate, window },
            burst: burst.map(|capacity| BurstCapacity { capacity }),
            scope: crate::domain::model::RateScope::Tenant,
            strategy: crate::domain::model::RateStrategy::Reject,
            cost: 1,
            response_headers: true,
        }
    }

    #[test]
    fn no_limits_yields_none() {
        assert!(effective_limit(&[], None, None).is_none());
    }

    #[test]
    fn ancestor_enforce_wins_over_looser_descendant() {
        let ancestor = limit(SharingMode::Enforce, 10, RateWindow::Second, Some(10));
        let own = limit(SharingMode::Inherit, 100, RateWindow::Second, Some(200));
        let chain = vec![LimitContribution {
            tenant_id: uuid::Uuid::nil(),
            limit: &ancestor,
        }];
        let effective = effective_limit(&chain, Some(&own), None).unwrap();
        assert_eq!(effective.sustained.rate, 10);
        assert_eq!(effective.capacity(), 10);
    }

    #[test]
    fn private_ancestor_limit_is_invisible() {
        let ancestor = limit(SharingMode::Private, 5, RateWindow::Second, None);
        let own = limit(SharingMode::Inherit, 100, RateWindow::Second, None);
        let chain = vec![LimitContribution {
            tenant_id: uuid::Uuid::new_v4(),
            limit: &ancestor,
        }];
        let effective = effective_limit(&chain, Some(&own), None).unwrap();
        assert_eq!(effective.sustained.rate, 100);
    }

    #[test]
    fn inherit_ancestor_is_overridden() {
        let ancestor = limit(SharingMode::Inherit, 50, RateWindow::Second, None);
        let own = limit(SharingMode::Private, 30, RateWindow::Second, None);
        let chain = vec![LimitContribution {
            tenant_id: uuid::Uuid::new_v4(),
            limit: &ancestor,
        }];
        let effective = effective_limit(&chain, Some(&own), None).unwrap();
        assert_eq!(effective.sustained.rate, 30);
    }

    #[test]
    fn inherit_ancestor_limit_applies_without_an_own_limit() {
        let ancestor = limit(SharingMode::Inherit, 50, RateWindow::Second, None);
        let chain = vec![LimitContribution {
            tenant_id: uuid::Uuid::new_v4(),
            limit: &ancestor,
        }];
        let effective = effective_limit(&chain, None, None).unwrap();
        assert_eq!(effective.sustained.rate, 50);
    }

    #[test]
    fn inherit_ancestor_limit_is_not_a_cap_for_an_own_limit() {
        let ancestor = limit(SharingMode::Inherit, 50, RateWindow::Second, None);
        let chain = vec![LimitContribution {
            tenant_id: uuid::Uuid::new_v4(),
            limit: &ancestor,
        }];
        let own = limit(SharingMode::Inherit, 30, RateWindow::Second, None);
        assert_eq!(
            effective_limit(&chain, Some(&own), None)
                .unwrap()
                .sustained
                .rate,
            30
        );
        // A looser descendant limit is not capped by an `inherit` ancestor.
        let looser = limit(SharingMode::Inherit, 80, RateWindow::Second, None);
        assert_eq!(
            effective_limit(&chain, Some(&looser), None)
                .unwrap()
                .sustained
                .rate,
            80
        );
    }

    #[test]
    fn inherit_ancestor_limit_is_min_with_enforce_and_route_limits() {
        let first = limit(SharingMode::Inherit, 50, RateWindow::Second, None);
        let second = limit(SharingMode::Inherit, 90, RateWindow::Second, None);
        let enforce = limit(SharingMode::Enforce, 40, RateWindow::Second, None);
        let route = limit(SharingMode::Private, 70, RateWindow::Second, None);
        let chain = vec![
            LimitContribution {
                tenant_id: uuid::Uuid::new_v4(),
                limit: &first,
            },
            LimitContribution {
                tenant_id: uuid::Uuid::new_v4(),
                limit: &enforce,
            },
            LimitContribution {
                tenant_id: uuid::Uuid::new_v4(),
                limit: &second,
            },
        ];
        // No own limit: the nearest `inherit` ancestor (90) is the default and
        // is min-ed with the `enforce` ancestor (40) and the route (70).
        let effective = effective_limit(&chain, None, Some(&route)).unwrap();
        assert_eq!(effective.sustained.rate, 40);
        assert_eq!(effective.capacity(), 40);
    }

    #[test]
    fn route_limit_is_min_with_the_own_limit_even_without_ancestors() {
        let own = limit(SharingMode::Inherit, 100, RateWindow::Second, None);
        let route = limit(SharingMode::Private, 20, RateWindow::Second, None);
        assert_eq!(
            effective_limit(&[], Some(&own), Some(&route))
                .unwrap()
                .sustained
                .rate,
            20
        );
        // No own limit either: the route limit alone applies.
        assert_eq!(
            effective_limit(&[], None, Some(&route))
                .unwrap()
                .sustained
                .rate,
            20
        );
    }

    #[test]
    fn route_limit_is_combined_with_upstream_limit() {
        let own = limit(SharingMode::Inherit, 100, RateWindow::Second, Some(500));
        let route = limit(SharingMode::Private, 20, RateWindow::Second, Some(40));
        let effective = effective_limit(&[], Some(&own), Some(&route)).unwrap();
        assert_eq!(effective.sustained.rate, 20);
        assert_eq!(effective.capacity(), 40);
    }

    #[test]
    fn retry_after_is_at_least_one_second() {
        let cfg = limit(SharingMode::Private, 1, RateWindow::Second, None);
        assert_eq!(cfg.retry_after_seconds(), 1);
        assert_eq!(cfg.refill_per_second(), 1.0);
    }
}
