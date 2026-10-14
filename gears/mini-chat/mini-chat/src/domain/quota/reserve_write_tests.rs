//! Reserve write tests over in-memory `SQLite`: the bucket increments per tier, the preflight
//! columns and `started_at` of a retry or edit turn, the re-check with rollback (no row changed,
//! no downgrade), two reserves racing for the last credits (sequential and concurrent), the
//! overflow-safe re-check, the period boundary and the reserve lifecycle states.
//!
//! Slice S1-all: algorithm `cpt-cf-mini-chat-algo-quota-and-billing-reserve-write`, flow
//! `cpt-cf-mini-chat-flow-quota-and-billing-turn-admission` (reserve steps), state
//! `cpt-cf-mini-chat-state-quota-and-billing-turn-reserve`, definitions of done
//! `cpt-cf-mini-chat-dod-quota-and-billing-reserve-write` and
//! `cpt-cf-mini-chat-dod-quota-and-billing-period-buckets`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use mini_chat_sdk::ModelTier;
use uuid::Uuid;

use crate::domain::error::{DomainError, QuotaScope};
use crate::domain::quota::{PeriodType, PreflightDecision};
use crate::test_support::chats::outbox_body_count;
use crate::test_support::quota::{
    FixedClock, MetricCall, PREMIUM, QuotaRig, QuotaRigOptions, STANDARD, TurnSeed, all_quota_rows,
    default_now, plain_facts, quota_rig, quota_rig_with, quota_row, reserved_of, row, seed_rows,
    seed_turn, spent_of, turn_row, utc, utc_ms,
};
use crate::test_support::{USER_A1, raw_i64};

const DAY: &str = "2026-02-28";
const MONTH: &str = "2026-02-01";

fn assert_tokens_rejection<T: std::fmt::Debug>(result: Result<T, DomainError>) {
    match result {
        Err(DomainError::QuotaExceeded {
            scope: QuotaScope::Tokens,
        }) => {}
        other => panic!("expected a tokens quota rejection, got {other:?}"),
    }
}

async fn turn_count(rig: &QuotaRig) -> i64 {
    raw_i64(&rig.db.raw, "SELECT COUNT(*) FROM chat_turns").await
}

// ── increments ───────────────────────────────────────────────────────────────

#[tokio::test]
async fn a_premium_turn_increments_the_total_and_the_premium_bucket_of_both_periods() {
    let rig = quota_rig().await;
    let chat = rig.chat_a1(PREMIUM).await;
    let booked = rig.book(&chat, PREMIUM, plain_facts()).await;
    assert_eq!(booked.decision.tier, ModelTier::Premium);
    assert_eq!(booked.decision.reserved_credits_micro, 3_750_000);

    for (period, start) in [("daily", DAY), ("monthly", MONTH)] {
        for bucket in ["total", "tier:premium"] {
            let row = quota_row(&rig.db, period, start, bucket)
                .await
                .unwrap_or_else(|| panic!("row {period} {bucket} missing"));
            assert_eq!(row.reserved, 3_750_000, "{period} {bucket}");
            assert_eq!(row.spent, 0, "{period} {bucket}");
            assert_eq!(row.calls, 0, "{period} {bucket}");
        }
    }
    assert_eq!(all_quota_rows(&rig.db).await.len(), 4);
}

#[tokio::test]
async fn a_standard_turn_increments_the_total_bucket_only() {
    let rig = quota_rig().await;
    let chat = rig.chat_a1(STANDARD).await;
    let booked = rig.book(&chat, STANDARD, plain_facts()).await;
    assert_eq!(booked.decision.tier, ModelTier::Standard);

    assert_eq!(reserved_of(&rig.db, "daily", DAY, "total").await, 1_500_000);
    assert_eq!(
        reserved_of(&rig.db, "monthly", MONTH, "total").await,
        1_500_000
    );
    let rows = all_quota_rows(&rig.db).await;
    assert_eq!(rows.len(), 2);
    assert!(
        rows.iter().all(|row| row.bucket == "total"),
        "no premium or tier:standard row: {rows:?}"
    );
}

#[tokio::test]
async fn the_increment_adds_to_an_existing_row_and_leaves_the_settled_counters_alone() {
    let rig = quota_rig().await;
    seed_rows(
        &rig.db,
        &[
            row("daily", DAY, "total")
                .spent(5_000_000)
                .reserved(1_000_000)
                .web(3),
            row("monthly", MONTH, "total").spent(40_000_000),
        ],
    )
    .await;
    let chat = rig.chat_a1(STANDARD).await;
    rig.book(&chat, STANDARD, plain_facts()).await;

    let daily = quota_row(&rig.db, "daily", DAY, "total").await.unwrap();
    assert_eq!(daily.reserved, 2_500_000);
    assert_eq!(daily.spent, 5_000_000);
    assert_eq!(daily.web_search_calls, 3);
    assert_eq!(daily.calls, 0);
    let monthly = quota_row(&rig.db, "monthly", MONTH, "total").await.unwrap();
    assert_eq!(monthly.reserved, 1_500_000);
    assert_eq!(monthly.spent, 40_000_000);
}

#[tokio::test]
async fn the_rows_of_another_user_are_untouched() {
    let rig = quota_rig().await;
    seed_rows(
        &rig.db,
        &[row("daily", DAY, "total")
            .spent(7)
            .of(crate::test_support::TENANT_A, crate::test_support::USER_A2)],
    )
    .await;
    let chat = rig.chat_a1(STANDARD).await;
    rig.book(&chat, STANDARD, plain_facts()).await;
    let other =
        crate::test_support::quota::quota_rows_of(&rig.db, crate::test_support::USER_A2).await;
    assert_eq!(other.len(), 1);
    assert_eq!(other[0].spent, 7);
    assert_eq!(other[0].reserved, 0);
}

#[tokio::test]
async fn the_receipt_names_the_tier_the_credits_and_both_periods() {
    let rig = quota_rig().await;
    let chat = rig.chat_a1(PREMIUM).await;
    let booked = rig.book(&chat, PREMIUM, plain_facts()).await;
    assert_eq!(booked.receipt.tier, ModelTier::Premium);
    assert_eq!(booked.receipt.reserved_credits_micro, 3_750_000);
    assert_eq!(
        booked.receipt.periods,
        vec![PeriodType::Daily, PeriodType::Monthly]
    );
}

// ── the turn columns and started_at ──────────────────────────────────────────

#[tokio::test]
async fn a_retry_or_edit_turn_gets_its_preflight_columns_and_the_preflight_instant() {
    let rig = quota_rig().await;
    let chat = rig.chat_a1(STANDARD).await;
    // the mutation flow committed the new turn earlier, with another `started_at`
    let mutation_instant = utc(2026, 2, 28, 15, 0, 0);
    let turn = TurnSeed::running(&chat, mutation_instant);
    let request_id = turn.request_id;
    let turn_id = seed_turn(&rig.db, &turn).await;

    let decision = rig.preflight(&chat, STANDARD, plain_facts()).await.unwrap();
    let mut identity = QuotaRig::identity(&chat);
    identity.request_id = request_id;
    rig.reserve_existing(&identity, &decision, turn_id)
        .await
        .unwrap();

    let row = turn_row(&rig.db, turn_id).await;
    assert_eq!(row.state, "running");
    assert_eq!(row.policy_version_applied, Some(1));
    assert_eq!(row.effective_model.as_deref(), Some(STANDARD));
    assert_eq!(row.reserve_tokens, Some(1500));
    assert_eq!(row.max_output_tokens_applied, Some(500));
    assert_eq!(row.reserved_credits_micro, Some(1_500_000));
    assert_eq!(row.minimal_generation_floor_applied, Some(50));
    assert_eq!(
        row.started_at,
        default_now(),
        "the preflight instant, not the mutation instant"
    );
    assert_eq!(row.request_id, request_id);
    assert_eq!(reserved_of(&rig.db, "daily", DAY, "total").await, 1_500_000);
}

#[tokio::test]
async fn the_preflight_columns_are_written_once_and_never_changed() {
    let rig = quota_rig().await;
    let chat = rig.chat_a1(STANDARD).await;
    let turn_id = seed_turn(
        &rig.db,
        &TurnSeed::running(&chat, utc(2026, 2, 28, 15, 0, 0)),
    )
    .await;
    let decision = rig.preflight(&chat, STANDARD, plain_facts()).await.unwrap();
    let identity = QuotaRig::identity(&chat);
    rig.reserve_existing(&identity, &decision, turn_id)
        .await
        .unwrap();
    let after_first = all_quota_rows(&rig.db).await;
    let columns = turn_row(&rig.db, turn_id).await;

    // a second write for the same turn is refused and rolled back: nothing is booked twice
    let again = rig.reserve_existing(&identity, &decision, turn_id).await;
    assert!(again.is_err(), "{again:?}");
    assert_eq!(all_quota_rows(&rig.db).await, after_first);
    assert_eq!(turn_row(&rig.db, turn_id).await, columns);
}

#[tokio::test]
async fn a_missing_turn_cannot_be_filled_and_books_nothing() {
    let rig = quota_rig().await;
    let chat = rig.chat_a1(STANDARD).await;
    let decision = rig.preflight(&chat, STANDARD, plain_facts()).await.unwrap();
    let result = rig
        .reserve_existing(&QuotaRig::identity(&chat), &decision, Uuid::new_v4())
        .await;
    assert!(result.is_err(), "{result:?}");
    assert!(all_quota_rows(&rig.db).await.is_empty());
}

#[tokio::test]
async fn the_send_path_turn_carries_the_columns_of_the_decision_and_the_preflight_instant() {
    let rig = quota_rig().await;
    let chat = rig.chat_a1(PREMIUM).await;
    let booked = rig.book(&chat, PREMIUM, plain_facts()).await;
    let row = turn_row(&rig.db, booked.turn_id).await;
    assert_eq!(row.state, "running");
    assert_eq!(row.effective_model.as_deref(), Some(PREMIUM));
    assert_eq!(row.reserved_credits_micro, Some(3_750_000));
    assert_eq!(row.started_at, booked.decision.started_at);
}

// ── the re-check ─────────────────────────────────────────────────────────────

/// Two decisions preflighted against empty rows with a daily total limit of 2,000,000 each
/// fit alone (1,500,000) but not together.
async fn racing_rig() -> (
    QuotaRig,
    PreflightDecision,
    PreflightDecision,
    QuotaRigChats,
) {
    let rig = quota_rig().await;
    rig.gateway
        .set_limits((2_000_000, 600_000_000), (22_000_000, 300_000_000));
    let first_chat = rig.chat_a1(STANDARD).await;
    let second_chat = rig.chat_a1(STANDARD).await;
    let first = rig
        .preflight(&first_chat, STANDARD, plain_facts())
        .await
        .unwrap();
    let second = rig
        .preflight(&second_chat, STANDARD, plain_facts())
        .await
        .unwrap();
    (
        rig,
        first,
        second,
        QuotaRigChats {
            first_chat,
            second_chat,
        },
    )
}

struct QuotaRigChats {
    first_chat: crate::test_support::quota::ChatRecord,
    second_chat: crate::test_support::quota::ChatRecord,
}

#[tokio::test]
async fn two_reserves_that_pass_preflight_but_exceed_the_limit_together_reject_the_later_one() {
    let (rig, first, second, chats) = racing_rig().await;
    rig.reserve_new(&QuotaRig::identity(&chats.first_chat), &first)
        .await
        .unwrap();
    let booked = all_quota_rows(&rig.db).await;
    let calls_before = rig.gateway.total_calls();

    let rejected = rig
        .reserve_new(&QuotaRig::identity(&chats.second_chat), &second)
        .await;
    assert_tokens_rejection(rejected);

    // the transaction rolled back: nothing of the second reserve remains
    assert_eq!(all_quota_rows(&rig.db).await, booked);
    assert_eq!(turn_count(&rig).await, 1, "no turn of the rejected reserve");
    // and no new cascade ran: the policy was not asked again
    assert_eq!(rig.gateway.total_calls(), calls_before);
}

#[tokio::test]
async fn two_concurrent_reserves_cannot_both_book_over_the_limit() {
    let (rig, first, second, chats) = racing_rig().await;
    let first_identity = QuotaRig::identity(&chats.first_chat);
    let second_identity = QuotaRig::identity(&chats.second_chat);
    let (a, b) = tokio::join!(
        rig.reserve_new(&first_identity, &first),
        rig.reserve_new(&second_identity, &second)
    );
    let outcomes = [a.is_ok(), b.is_ok()];
    assert_eq!(outcomes.iter().filter(|ok| **ok).count(), 1, "{outcomes:?}");
    let failed = if a.is_err() { a } else { b };
    assert_tokens_rejection(failed);
    assert_eq!(reserved_of(&rig.db, "daily", DAY, "total").await, 1_500_000);
    assert_eq!(turn_count(&rig).await, 1);
}

#[tokio::test]
async fn a_rejected_re_check_leaves_every_row_and_turn_unchanged_for_a_retry_or_edit() {
    let (rig, first, second, chats) = racing_rig().await;
    rig.reserve_new(&QuotaRig::identity(&chats.first_chat), &first)
        .await
        .unwrap();
    let booked = all_quota_rows(&rig.db).await;

    let turn_id = seed_turn(
        &rig.db,
        &TurnSeed::running(&chats.second_chat, utc(2026, 2, 28, 15, 0, 0)),
    )
    .await;
    let before = turn_row(&rig.db, turn_id).await;
    let rejected = rig
        .reserve_existing(&QuotaRig::identity(&chats.second_chat), &second, turn_id)
        .await;
    assert_tokens_rejection(rejected);
    assert_eq!(all_quota_rows(&rig.db).await, booked);
    let after = turn_row(&rig.db, turn_id).await;
    assert_eq!(
        after, before,
        "the NULL columns and started_at stay as the mutation left them"
    );
    assert_eq!(after.reserve_tokens, None);
    assert_eq!(after.state, "running");
}

#[tokio::test]
async fn the_premium_sub_cap_is_re_checked_and_the_total_increments_roll_back() {
    let rig = quota_rig().await;
    let chat = rig.chat_a1(PREMIUM).await;
    let decision = rig.preflight(&chat, PREMIUM, plain_facts()).await.unwrap();
    // a concurrent premium turn settled after the preflight: 20_000_000 + 3_750_000 > 22_000_000
    seed_rows(
        &rig.db,
        &[row("daily", DAY, "tier:premium").spent(20_000_000)],
    )
    .await;
    let before = all_quota_rows(&rig.db).await;

    assert_tokens_rejection(rig.reserve_new(&QuotaRig::identity(&chat), &decision).await);
    assert_eq!(
        all_quota_rows(&rig.db).await,
        before,
        "no total row was left behind"
    );
}

#[tokio::test]
async fn a_monthly_bucket_over_its_limit_rejects_although_the_daily_one_fits() {
    let rig = quota_rig().await;
    let chat = rig.chat_a1(STANDARD).await;
    let decision = rig.preflight(&chat, STANDARD, plain_facts()).await.unwrap();
    seed_rows(
        &rig.db,
        &[row("monthly", MONTH, "total").spent(599_000_000)],
    )
    .await;
    let before = all_quota_rows(&rig.db).await;
    assert_tokens_rejection(rig.reserve_new(&QuotaRig::identity(&chat), &decision).await);
    assert_eq!(all_quota_rows(&rig.db).await, before);
}

#[tokio::test]
async fn a_reserve_that_fits_exactly_is_booked() {
    let rig = quota_rig().await;
    rig.gateway
        .set_limits((1_500_000, 1_500_000), (22_000_000, 300_000_000));
    let chat = rig.chat_a1(STANDARD).await;
    let decision = rig.preflight(&chat, STANDARD, plain_facts()).await.unwrap();
    rig.reserve_new(&QuotaRig::identity(&chat), &decision)
        .await
        .unwrap();
    assert_eq!(reserved_of(&rig.db, "daily", DAY, "total").await, 1_500_000);
}

#[tokio::test]
async fn the_limits_carried_by_the_decision_are_used_not_a_new_policy_read() {
    let rig = quota_rig().await;
    let chat = rig.chat_a1(STANDARD).await;
    let decision = rig.preflight(&chat, STANDARD, plain_facts()).await.unwrap();
    // the policy changes after the preflight: the reserve write still uses the decision
    rig.gateway.set_limits((1, 1), (1, 1));
    let calls_before = rig.gateway.total_calls();
    rig.reserve_new(&QuotaRig::identity(&chat), &decision)
        .await
        .unwrap();
    assert_eq!(rig.gateway.total_calls(), calls_before);
}

// ── overflow ─────────────────────────────────────────────────────────────────

#[tokio::test]
async fn an_increment_that_would_overflow_the_counter_is_a_rejection_not_a_wrap_or_a_failure() {
    let rig = quota_rig().await;
    let chat = rig.chat_a1(STANDARD).await;
    let decision = rig.preflight(&chat, STANDARD, plain_facts()).await.unwrap();
    seed_rows(
        &rig.db,
        &[row("daily", DAY, "total").reserved(i64::MAX - 10)],
    )
    .await;
    let before = all_quota_rows(&rig.db).await;
    assert_tokens_rejection(rig.reserve_new(&QuotaRig::identity(&chat), &decision).await);
    assert_eq!(all_quota_rows(&rig.db).await, before);
}

#[tokio::test]
async fn spent_plus_reserved_beyond_i64_max_is_compared_in_wide_arithmetic() {
    let rig = quota_rig().await;
    let chat = rig.chat_a1(STANDARD).await;
    let decision = rig.preflight(&chat, STANDARD, plain_facts()).await.unwrap();
    seed_rows(
        &rig.db,
        &[row("monthly", MONTH, "total").spent(i64::MAX - 1)],
    )
    .await;
    let before = all_quota_rows(&rig.db).await;
    assert_tokens_rejection(rig.reserve_new(&QuotaRig::identity(&chat), &decision).await);
    assert_eq!(all_quota_rows(&rig.db).await, before);
}

#[tokio::test]
async fn a_failed_computation_sentinel_is_never_booked_and_never_wraps_the_check() {
    let rig = quota_rig().await;
    let chat = rig.chat_a1(STANDARD).await;
    let mut decision = rig.preflight(&chat, STANDARD, plain_facts()).await.unwrap();
    decision.reserved_credits_micro = i64::MAX;
    seed_rows(
        &rig.db,
        &[
            row("daily", DAY, "total").spent(10).reserved(20),
            row("monthly", MONTH, "total").spent(30).reserved(40),
        ],
    )
    .await;
    let before = all_quota_rows(&rig.db).await;
    assert_tokens_rejection(rig.reserve_new(&QuotaRig::identity(&chat), &decision).await);
    assert_eq!(all_quota_rows(&rig.db).await, before);
    assert_eq!(turn_count(&rig).await, 0);
}

// ── the period boundary ──────────────────────────────────────────────────────

#[tokio::test]
async fn a_reserve_at_the_last_millisecond_of_a_day_targets_the_rows_of_that_day() {
    let clock = FixedClock::at(utc_ms(2026, 2, 28, 23, 59, 59, 999));
    let rig = quota_rig_with(QuotaRigOptions {
        clock,
        ..QuotaRigOptions::new()
    })
    .await;
    let chat = rig.chat_a1(STANDARD).await;
    let booked = rig.book(&chat, STANDARD, plain_facts()).await;
    assert_eq!(reserved_of(&rig.db, "daily", DAY, "total").await, 1_500_000);
    assert_eq!(
        reserved_of(&rig.db, "monthly", MONTH, "total").await,
        1_500_000
    );
    assert!(
        quota_row(&rig.db, "daily", "2026-03-01", "total")
            .await
            .is_none()
    );
    let row = turn_row(&rig.db, booked.turn_id).await;
    assert_eq!(row.started_at, utc_ms(2026, 2, 28, 23, 59, 59, 999));
}

// ── metrics ──────────────────────────────────────────────────────────────────

#[tokio::test]
async fn the_reserve_metric_is_recorded_per_period_only_after_the_commit() {
    let rig = quota_rig().await;
    let chat = rig.chat_a1(STANDARD).await;
    let decision = rig.preflight(&chat, STANDARD, plain_facts()).await.unwrap();
    rig.metrics.clear();
    let (receipt, _) = rig
        .reserve_new(&QuotaRig::identity(&chat), &decision)
        .await
        .unwrap();
    assert!(
        rig.metrics.calls().is_empty(),
        "the write itself records nothing"
    );
    receipt.record_after_commit(rig.metrics.as_ref());
    assert_eq!(
        rig.metrics.calls(),
        vec![
            MetricCall::Reserve(PeriodType::Daily),
            MetricCall::Reserve(PeriodType::Monthly)
        ]
    );
}

#[tokio::test]
async fn a_rejected_reserve_records_no_reserve_metric() {
    let (rig, first, second, chats) = racing_rig().await;
    rig.reserve_new(&QuotaRig::identity(&chats.first_chat), &first)
        .await
        .unwrap();
    rig.metrics.clear();
    assert!(
        rig.reserve_new(&QuotaRig::identity(&chats.second_chat), &second)
            .await
            .is_err()
    );
    assert!(
        !rig.metrics
            .calls()
            .iter()
            .any(|call| matches!(call, MetricCall::Reserve(_)))
    );
}

// ── the reserve lifecycle ────────────────────────────────────────────────────

#[tokio::test]
async fn reserve_state_no_reserve_to_reserved_when_the_write_commits() {
    let rig = quota_rig().await;
    let chat = rig.chat_a1(STANDARD).await;
    assert_eq!(turn_count(&rig).await, 0);
    let booked = rig.book(&chat, STANDARD, plain_facts()).await;
    let row = turn_row(&rig.db, booked.turn_id).await;
    assert_eq!(row.state, "running");
    assert!(row.reserve_tokens.is_some());
    assert_eq!(reserved_of(&rig.db, "daily", DAY, "total").await, 1_500_000);
}

#[tokio::test]
async fn reserve_state_no_reserve_stays_after_a_rejection_with_no_settlement_or_usage_event() {
    let (rig, first, second, chats) = racing_rig().await;
    rig.reserve_new(&QuotaRig::identity(&chats.first_chat), &first)
        .await
        .unwrap();
    let before = all_quota_rows(&rig.db).await;
    assert!(
        rig.reserve_new(&QuotaRig::identity(&chats.second_chat), &second)
            .await
            .is_err()
    );
    assert_eq!(all_quota_rows(&rig.db).await, before);
    assert_eq!(
        outbox_body_count(&rig.db).await,
        0,
        "no usage event for a reserve that never existed"
    );
    assert_eq!(spent_of(&rig.db, "daily", DAY, "total").await, 0);
    assert_eq!(
        rig.metrics
            .calls()
            .iter()
            .filter(|c| matches!(c, MetricCall::Commit(_)))
            .count(),
        0
    );
    let _ = USER_A1;
}
