#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Slice 3: the pure parts of `cpt-cf-mini-chat-algo-quota-billing-reserve-write`
//! (the six column values, the buckets written and the re-check). The
//! transaction behaviour is tested on `SQLite` in
//! `infra::db::repos::quota_reserve_tests`.

use std::time::Duration;

use mini_chat_sdk::ModelTier;

use super::{
    CONFLICT_BACKOFF, MAX_TX_ATTEMPTS, ReservedColumns, reserve_buckets, reserve_exceeds_limit,
};
use crate::domain::service::quota::preflight::QuotaDecision;
use crate::domain::service::quota::test_support::{design_fixture, design_limits, limits, request};
use crate::domain::service::quota::types::BucketRow;
use crate::domain::service::quota::types::QuotaBucket::{TierPremium, Total};
use crate::domain::service::quota::types::QuotaPeriod::{Daily, Monthly};

#[test]
fn a_conflict_is_retried_three_attempts_in_total_with_a_short_backoff() {
    assert_eq!(MAX_TX_ATTEMPTS, 3);
    assert_eq!(
        CONFLICT_BACKOFF,
        [Duration::from_millis(20), Duration::from_millis(100)]
    );
}

#[tokio::test]
async fn the_six_columns_come_from_the_preflight_decision() {
    let fixture = design_fixture();
    let decision = fixture.service.preflight(&request("p")).await.unwrap();

    let columns = ReservedColumns::from_decision(&decision);

    // DESIGN 5.10: the premium model is exhausted, the standard one runs.
    assert_eq!(decision.quota_decision, QuotaDecision::Downgrade);
    assert_eq!(
        columns,
        ReservedColumns {
            reserve_tokens: 1500,
            max_output_tokens_applied: 500,
            reserved_credits_micro: 1_500_000,
            policy_version_applied: 7,
            effective_model: "s".to_owned(),
            minimal_generation_floor_applied: 50,
        }
    );
}

#[test]
fn a_premium_turn_reserves_total_then_tier_premium_and_a_standard_turn_total_only() {
    assert_eq!(reserve_buckets(ModelTier::Premium), [Total, TierPremium]);
    assert_eq!(reserve_buckets(ModelTier::Standard), [Total]);
}

#[test]
fn the_recheck_passes_at_the_limit_and_fails_one_micro_credit_above() {
    let limits = limits(7, 1_000, 10_000, 500, 5_000);
    let at_limit = vec![
        BucketRow::new(Daily, Total, 600, 400),
        BucketRow::new(Monthly, Total, 10_000, 0),
        BucketRow::new(Daily, TierPremium, 0, 500),
        BucketRow::new(Monthly, TierPremium, 5_000, 0),
    ];
    assert!(!reserve_exceeds_limit(&at_limit, &limits));

    for (index, over) in [
        BucketRow::new(Daily, Total, 600, 401),
        BucketRow::new(Monthly, Total, 10_001, 0),
        BucketRow::new(Daily, TierPremium, 0, 501),
        BucketRow::new(Monthly, TierPremium, 5_000, 1),
    ]
    .into_iter()
    .enumerate()
    {
        let mut rows = at_limit.clone();
        rows[index] = over;

        assert!(reserve_exceeds_limit(&rows, &limits), "row {index}");
    }
}

#[test]
fn the_recheck_maps_each_bucket_to_its_own_limit() {
    // total = standard limits (large), premium limits tiny
    let limits = limits(7, 1_000_000, 1_000_000, 10, 10);

    assert!(!reserve_exceeds_limit(
        &[BucketRow::new(Daily, Total, 999_999, 0)],
        &limits
    ));
    assert!(reserve_exceeds_limit(
        &[BucketRow::new(Daily, TierPremium, 11, 0)],
        &limits
    ));
}

#[test]
fn rows_of_a_standard_turn_only_need_the_total_limits() {
    let limits = design_limits(7);
    let rows = vec![
        BucketRow::new(Daily, Total, 26_500_000, 0),
        BucketRow::new(Monthly, Total, 241_500_000, 0),
    ];

    assert!(!reserve_exceeds_limit(&rows, &limits));
    assert!(!reserve_exceeds_limit(&[], &limits));
}

#[test]
fn an_overflowing_sum_counts_as_over_the_limit() {
    let limits = limits(7, i64::MAX, i64::MAX, i64::MAX, i64::MAX);

    assert!(reserve_exceeds_limit(
        &[BucketRow::new(Daily, Total, i64::MAX, 1)],
        &limits
    ));
}
