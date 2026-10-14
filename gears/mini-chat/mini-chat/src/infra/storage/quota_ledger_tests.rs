//! Ledger tests over in-memory `SQLite`: the owner-scoped read of the bucket rows, the atomic
//! reserve increment with its overflow result, the preflight columns written once, the CAS
//! guard shapes of the stream and the orphan finalizer, the turn load and the settlement delta.
//!
//! Slice S1-all: algorithms `cpt-cf-mini-chat-algo-quota-and-billing-reserve-write`,
//! `cpt-cf-mini-chat-algo-quota-and-billing-finalization-settlement`,
//! `cpt-cf-mini-chat-algo-quota-and-billing-token-estimation` (prior context) and flow
//! `cpt-cf-mini-chat-flow-quota-and-billing-quota-status` (`inst-status-read-rows`), definitions
//! of done `cpt-cf-mini-chat-dod-quota-and-billing-period-buckets` and
//! `cpt-cf-mini-chat-dod-quota-and-billing-cas-exactly-once`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use sea_orm_migration::MigratorTrait;
use time::OffsetDateTime;
use toolkit_security::AccessScope;
use uuid::Uuid;

use super::SqlQuotaLedger;
use crate::domain::error::DomainError;
use crate::domain::quota::{
    Bucket, BucketKey, PeriodStarts, PeriodType, QuotaLedger, QuotaRows, ReserveIncrement,
    SettleDelta, TerminalState, TurnPreflightColumns, TurnRecord, TurnTerminalUpdate,
};
use crate::infra::storage::migrations::Migrator;
use crate::test_support::chats::{a1, at};
use crate::test_support::quota::{
    ReserveCols, TurnSeed, all_quota_rows, date, quota_row, row, seed_message_with_tokens,
    seed_rows, seed_turn, touch_turn, turn_row, utc,
};
use crate::test_support::{TENANT_A, TENANT_B, TestDb, USER_A1, USER_A2, sqlite_db};

const DAY: &str = "2026-02-28";
const MONTH: &str = "2026-02-01";

async fn setup() -> TestDb {
    sqlite_db(Migrator::migrations()).await
}

fn periods() -> PeriodStarts {
    PeriodStarts::of(utc(2026, 2, 28, 15, 30, 0))
}

fn key(period: PeriodType, bucket: Bucket) -> BucketKey {
    BucketKey {
        tenant_id: TENANT_A,
        user_id: USER_A1,
        period,
        period_start: if period == PeriodType::Daily {
            date(2026, 2, 28)
        } else {
            date(2026, 2, 1)
        },
        bucket,
    }
}

fn user_scope() -> AccessScope {
    AccessScope::for_tenant(TENANT_A).ensure_owner(USER_A1)
}

async fn read_rows(db: &TestDb, scope: AccessScope) -> Result<QuotaRows, DomainError> {
    let starts = periods();
    db.provider
        .transaction(move |tx| {
            Box::pin(async move { SqlQuotaLedger.read_rows(tx, &scope, &starts).await })
        })
        .await
}

async fn add_reserved(
    db: &TestDb,
    key: BucketKey,
    credits: i64,
) -> Result<ReserveIncrement, DomainError> {
    db.provider
        .transaction(move |tx| {
            Box::pin(async move { SqlQuotaLedger.add_reserved(tx, &key, credits).await })
        })
        .await
}

async fn cas(
    db: &TestDb,
    turn_id: Uuid,
    tenant: Uuid,
    state: TerminalState,
    stale_cutoff: Option<OffsetDateTime>,
) -> bool {
    let update = TurnTerminalUpdate {
        state,
        error_code: Some("provider_error".to_owned()),
        error_detail: Some("detail".to_owned()),
        assistant_message_id: Some(Uuid::from_u128(77)),
        completed_at: utc(2026, 2, 28, 16, 0, 0),
    };
    db.provider
        .transaction(move |tx| {
            Box::pin(async move {
                SqlQuotaLedger
                    .cas_terminal(tx, tenant, turn_id, &update, stale_cutoff)
                    .await
            })
        })
        .await
        .unwrap()
}

fn cols() -> ReserveCols {
    ReserveCols {
        policy_version: 4,
        effective_model: "standard-1".to_owned(),
        reserve_tokens: 1500,
        max_output_tokens: 500,
        reserved_credits_micro: 1_500_000,
        floor: 50,
    }
}

// ── read_rows ────────────────────────────────────────────────────────────────

#[tokio::test]
async fn the_read_returns_both_buckets_of_the_current_periods_through_the_owner_scope() {
    let db = setup().await;
    seed_rows(
        &db,
        &[
            row("daily", DAY, "total")
                .spent(10)
                .reserved(2)
                .web(3)
                .code(4),
            row("monthly", MONTH, "total").spent(100),
            row("daily", DAY, "tier:premium").spent(7),
            row("monthly", MONTH, "tier:premium").reserved(9),
            // another period start and another owner or tenant never appear
            row("daily", "2026-02-27", "total").spent(999),
            row("monthly", "2026-01-01", "total").spent(999),
            row("daily", DAY, "total").spent(555).of(TENANT_A, USER_A2),
            row("daily", DAY, "total").spent(444).of(TENANT_B, USER_A1),
        ],
    )
    .await;
    let rows = read_rows(&db, user_scope()).await.unwrap();
    let daily_total = rows.get(PeriodType::Daily, Bucket::Total);
    assert_eq!(
        (
            daily_total.spent_credits_micro,
            daily_total.reserved_credits_micro
        ),
        (10, 2)
    );
    assert_eq!(
        (
            daily_total.web_search_calls,
            daily_total.code_interpreter_calls
        ),
        (3, 4)
    );
    assert_eq!(
        rows.get(PeriodType::Monthly, Bucket::Total)
            .spent_credits_micro,
        100
    );
    assert_eq!(
        rows.get(PeriodType::Daily, Bucket::TierPremium)
            .spent_credits_micro,
        7
    );
    assert_eq!(
        rows.get(PeriodType::Monthly, Bucket::TierPremium)
            .reserved_credits_micro,
        9
    );
}

#[tokio::test]
async fn a_missing_row_reads_as_zero() {
    let db = setup().await;
    let rows = read_rows(&db, user_scope()).await.unwrap();
    for period in PeriodType::ALL {
        for bucket in [Bucket::Total, Bucket::TierPremium] {
            assert_eq!(
                rows.get(period, bucket),
                crate::domain::quota::BucketRow::default()
            );
        }
    }
}

#[tokio::test]
async fn the_rows_of_another_tenant_never_appear() {
    let db = setup().await;
    seed_rows(
        &db,
        &[row("daily", DAY, "total").spent(444).of(TENANT_B, USER_A1)],
    )
    .await;
    let rows = read_rows(&db, user_scope()).await.unwrap();
    assert_eq!(
        rows.get(PeriodType::Daily, Bucket::Total)
            .spent_credits_micro,
        0
    );
}

#[tokio::test]
async fn the_owner_predicate_hides_the_rows_of_other_users_of_the_tenant() {
    let db = setup().await;
    seed_rows(
        &db,
        &[row("daily", DAY, "total").spent(555).of(TENANT_A, USER_A2)],
    )
    .await;
    let rows = read_rows(&db, user_scope()).await.unwrap();
    assert_eq!(
        rows.get(PeriodType::Daily, Bucket::Total)
            .spent_credits_micro,
        0
    );
}

// ── add_reserved ─────────────────────────────────────────────────────────────

#[tokio::test]
async fn the_first_increment_creates_the_row_and_later_ones_add_to_it() {
    let db = setup().await;
    let target = key(PeriodType::Daily, Bucket::Total);
    assert_eq!(
        add_reserved(&db, target, 1_500_000).await.unwrap(),
        ReserveIncrement::Applied
    );
    assert_eq!(
        add_reserved(&db, target, 250_000).await.unwrap(),
        ReserveIncrement::Applied
    );
    let rows = all_quota_rows(&db).await;
    assert_eq!(rows.len(), 1, "one row per key: {rows:?}");
    let row = quota_row(&db, "daily", DAY, "total").await.unwrap();
    assert_eq!((row.reserved, row.spent, row.calls), (1_750_000, 0, 0));
    assert_eq!(row.bucket, "total");
}

#[tokio::test]
async fn an_increment_leaves_the_other_counters_and_rows_alone() {
    let db = setup().await;
    seed_rows(
        &db,
        &[
            row("daily", DAY, "total").spent(50).web(2),
            row("monthly", MONTH, "total").spent(60),
        ],
    )
    .await;
    add_reserved(&db, key(PeriodType::Daily, Bucket::Total), 10)
        .await
        .unwrap();
    let daily = quota_row(&db, "daily", DAY, "total").await.unwrap();
    assert_eq!(
        (daily.spent, daily.reserved, daily.web_search_calls),
        (50, 10, 2)
    );
    assert_eq!(
        quota_row(&db, "monthly", MONTH, "total")
            .await
            .unwrap()
            .reserved,
        0
    );
}

#[tokio::test]
async fn an_increment_that_would_overflow_is_reported_and_changes_nothing() {
    let db = setup().await;
    seed_rows(&db, &[row("daily", DAY, "total").reserved(i64::MAX - 10)]).await;
    let outcome = add_reserved(&db, key(PeriodType::Daily, Bucket::Total), 100)
        .await
        .unwrap();
    assert_eq!(outcome, ReserveIncrement::Overflow);
    assert_eq!(
        quota_row(&db, "daily", DAY, "total")
            .await
            .unwrap()
            .reserved,
        i64::MAX - 10
    );
}

// ── prior context ────────────────────────────────────────────────────────────

#[tokio::test]
async fn the_prior_context_is_zero_without_a_message_and_otherwise_the_latest_assistant_tokens() {
    let db = setup().await;
    let chat =
        crate::test_support::chats::seed_chat(&db, &a1(), Uuid::new_v4(), None, "m", at(0)).await;
    let read = |db: &TestDb, tenant: Uuid, chat_id: Uuid| {
        let provider = db.provider.clone();
        async move {
            provider
                .transaction(move |tx| {
                    Box::pin(async move {
                        SqlQuotaLedger
                            .prior_context_tokens(tx, tenant, chat_id)
                            .await
                    })
                })
                .await
                .unwrap()
        }
    };
    assert_eq!(read(&db, TENANT_A, chat.id).await, 0);
    seed_message_with_tokens(&db, &chat, "assistant", at(1), 10, 5, false).await;
    seed_message_with_tokens(&db, &chat, "assistant", at(2), 300, 200, false).await;
    assert_eq!(read(&db, TENANT_A, chat.id).await, 500);
    assert_eq!(
        read(&db, TENANT_B, chat.id).await,
        0,
        "the tenant is part of the filter"
    );
}

// ── fill_preflight_columns ───────────────────────────────────────────────────

async fn fill(db: &TestDb, tenant: Uuid, turn_id: Uuid, started_at: OffsetDateTime) -> bool {
    let columns = TurnPreflightColumns {
        policy_version_applied: 4,
        effective_model: "standard-1".to_owned(),
        reserve_tokens: 1500,
        max_output_tokens_applied: 500,
        reserved_credits_micro: 1_500_000,
        minimal_generation_floor_applied: 50,
        started_at,
    };
    db.provider
        .transaction(move |tx| {
            Box::pin(async move {
                SqlQuotaLedger
                    .fill_preflight_columns(tx, tenant, turn_id, &columns)
                    .await
            })
        })
        .await
        .unwrap()
}

#[tokio::test]
async fn the_null_columns_of_a_running_turn_are_filled_once_with_the_new_started_at() {
    let db = setup().await;
    let chat =
        crate::test_support::chats::seed_chat(&db, &a1(), Uuid::new_v4(), None, "m", at(0)).await;
    let turn_id = seed_turn(&db, &TurnSeed::running(&chat, utc(2026, 2, 28, 15, 0, 0))).await;
    let before = turn_row(&db, turn_id).await;

    assert!(fill(&db, TENANT_A, turn_id, utc(2026, 2, 28, 15, 30, 0)).await);
    let after = turn_row(&db, turn_id).await;
    assert_eq!(after.policy_version_applied, Some(4));
    assert_eq!(after.effective_model.as_deref(), Some("standard-1"));
    assert_eq!(after.reserve_tokens, Some(1500));
    assert_eq!(after.max_output_tokens_applied, Some(500));
    assert_eq!(after.reserved_credits_micro, Some(1_500_000));
    assert_eq!(after.minimal_generation_floor_applied, Some(50));
    assert_eq!(after.started_at, utc(2026, 2, 28, 15, 30, 0));
    assert_eq!(after.state, "running");
    assert_eq!(after.request_id, before.request_id);

    // never changed afterwards
    assert!(!fill(&db, TENANT_A, turn_id, utc(2026, 2, 28, 16, 0, 0)).await);
    assert_eq!(turn_row(&db, turn_id).await, after);
}

#[tokio::test]
async fn a_missing_foreign_or_finished_turn_is_not_filled() {
    let db = setup().await;
    let chat =
        crate::test_support::chats::seed_chat(&db, &a1(), Uuid::new_v4(), None, "m", at(0)).await;
    let turn_id = seed_turn(&db, &TurnSeed::running(&chat, utc(2026, 2, 28, 15, 0, 0))).await;
    assert!(!fill(&db, TENANT_A, Uuid::new_v4(), utc(2026, 2, 28, 15, 30, 0)).await);
    assert!(!fill(&db, TENANT_B, turn_id, utc(2026, 2, 28, 15, 30, 0)).await);
    assert!(cas(&db, turn_id, TENANT_A, TerminalState::Failed, None).await);
    assert!(!fill(&db, TENANT_A, turn_id, utc(2026, 2, 28, 15, 30, 0)).await);
    assert_eq!(turn_row(&db, turn_id).await.reserve_tokens, None);
}

// ── the CAS ──────────────────────────────────────────────────────────────────

#[tokio::test]
async fn the_cas_moves_a_running_turn_to_the_terminal_state_with_its_columns_exactly_once() {
    let db = setup().await;
    let chat =
        crate::test_support::chats::seed_chat(&db, &a1(), Uuid::new_v4(), None, "m", at(0)).await;
    let turn_id = seed_turn(
        &db,
        &TurnSeed::running(&chat, utc(2026, 2, 28, 15, 0, 0)).reserved(cols()),
    )
    .await;

    assert!(cas(&db, turn_id, TENANT_A, TerminalState::Failed, None).await);
    let turn = turn_row(&db, turn_id).await;
    assert_eq!(turn.state, "failed");
    assert_eq!(turn.error_code.as_deref(), Some("provider_error"));
    assert_eq!(turn.error_detail.as_deref(), Some("detail"));
    assert_eq!(turn.assistant_message_id, Some(Uuid::from_u128(77)));
    assert_eq!(turn.completed_at, Some(utc(2026, 2, 28, 16, 0, 0)));
    // the billing columns are untouched by the CAS
    assert_eq!(turn.reserve_tokens, Some(1500));

    for state in [
        TerminalState::Completed,
        TerminalState::Cancelled,
        TerminalState::Failed,
    ] {
        assert!(!cas(&db, turn_id, TENANT_A, state, None).await, "{state:?}");
    }
    assert_eq!(turn_row(&db, turn_id).await, turn);
}

#[tokio::test]
async fn the_cas_of_another_tenant_or_of_an_unknown_turn_wins_nothing() {
    let db = setup().await;
    let chat =
        crate::test_support::chats::seed_chat(&db, &a1(), Uuid::new_v4(), None, "m", at(0)).await;
    let turn_id = seed_turn(&db, &TurnSeed::running(&chat, utc(2026, 2, 28, 15, 0, 0))).await;
    assert!(!cas(&db, turn_id, TENANT_B, TerminalState::Completed, None).await);
    assert!(
        !cas(
            &db,
            Uuid::new_v4(),
            TENANT_A,
            TerminalState::Completed,
            None
        )
        .await
    );
    assert_eq!(turn_row(&db, turn_id).await.state, "running");
}

#[tokio::test]
async fn the_orphan_cas_requires_a_stale_turn_that_is_not_soft_deleted() {
    let db = setup().await;
    let chat =
        crate::test_support::chats::seed_chat(&db, &a1(), Uuid::new_v4(), None, "m", at(0)).await;
    let cutoff = Some(utc(2026, 2, 28, 15, 40, 0));

    // without progress the start counts: started after the cutoff is not stale
    let fresh = seed_turn(&db, &TurnSeed::running(&chat, utc(2026, 2, 28, 15, 50, 0))).await;
    assert!(!cas(&db, fresh, TENANT_A, TerminalState::Failed, cutoff).await);
    // started before the cutoff and no progress: stale
    let chat2 =
        crate::test_support::chats::seed_chat(&db, &a1(), Uuid::new_v4(), None, "m", at(0)).await;
    let stale = seed_turn(&db, &TurnSeed::running(&chat2, utc(2026, 2, 28, 15, 0, 0))).await;
    // recent progress keeps it alive, progress before the cutoff does not
    touch_turn(&db, stale, Some(utc(2026, 2, 28, 15, 45, 0)), None).await;
    assert!(!cas(&db, stale, TENANT_A, TerminalState::Failed, cutoff).await);
    touch_turn(
        &db,
        stale,
        Some(utc(2026, 2, 28, 15, 10, 0)),
        Some(utc(2026, 2, 28, 15, 12, 0)),
    )
    .await;
    assert!(
        !cas(&db, stale, TENANT_A, TerminalState::Failed, cutoff).await,
        "soft-deleted"
    );
    touch_turn(&db, stale, Some(utc(2026, 2, 28, 15, 10, 0)), None).await;
    assert!(cas(&db, stale, TENANT_A, TerminalState::Failed, cutoff).await);
    assert_eq!(turn_row(&db, fresh).await.state, "running");
}

#[tokio::test]
async fn the_stream_cas_is_guarded_by_the_running_state_alone() {
    let db = setup().await;
    let chat =
        crate::test_support::chats::seed_chat(&db, &a1(), Uuid::new_v4(), None, "m", at(0)).await;
    let turn_id = seed_turn(&db, &TurnSeed::running(&chat, utc(2026, 2, 28, 15, 50, 0))).await;
    touch_turn(
        &db,
        turn_id,
        Some(utc(2026, 2, 28, 15, 55, 0)),
        Some(utc(2026, 2, 28, 15, 56, 0)),
    )
    .await;
    assert!(cas(&db, turn_id, TENANT_A, TerminalState::Completed, None).await);
}

// ── load_turn ────────────────────────────────────────────────────────────────

#[tokio::test]
async fn the_turn_is_loaded_with_its_billing_columns_inside_the_tenant() {
    let db = setup().await;
    let chat =
        crate::test_support::chats::seed_chat(&db, &a1(), Uuid::new_v4(), None, "m", at(0)).await;
    let seed = TurnSeed::running(&chat, utc(2026, 2, 28, 15, 0, 0)).reserved(cols());
    let turn_id = seed_turn(&db, &seed).await;
    let load = |tenant: Uuid, id: Uuid| {
        let provider = db.provider.clone();
        async move {
            provider
                .transaction(move |tx| {
                    Box::pin(async move { SqlQuotaLedger.load_turn(tx, tenant, id).await })
                })
                .await
                .unwrap()
        }
    };
    let turn: TurnRecord = load(TENANT_A, turn_id).await.expect("the turn");
    assert_eq!(turn.id, turn_id);
    assert_eq!(turn.tenant_id, TENANT_A);
    assert_eq!(turn.chat_id, chat.id);
    assert_eq!(turn.request_id, seed.request_id);
    assert_eq!(turn.requester_user_id, Some(USER_A1));
    assert_eq!(turn.started_at, utc(2026, 2, 28, 15, 0, 0));
    assert_eq!(turn.reserve_tokens, Some(1500));
    assert_eq!(turn.max_output_tokens_applied, Some(500));
    assert_eq!(turn.reserved_credits_micro, Some(1_500_000));
    assert_eq!(turn.policy_version_applied, Some(4));
    assert_eq!(turn.effective_model.as_deref(), Some("standard-1"));
    assert_eq!(turn.minimal_generation_floor_applied, Some(50));
    assert!(load(TENANT_B, turn_id).await.is_none());
    assert!(load(TENANT_A, Uuid::new_v4()).await.is_none());
}

// ── settle_bucket ────────────────────────────────────────────────────────────

#[tokio::test]
async fn the_settlement_delta_moves_the_reserve_to_spent_and_adds_the_counters_in_one_statement() {
    let db = setup().await;
    seed_rows(
        &db,
        &[row("daily", DAY, "total")
            .spent(25_000_000)
            .reserved(1_500_000)
            .web(1)
            .code(2)],
    )
    .await;
    let delta = SettleDelta {
        release_reserved_credits_micro: 1_500_000,
        spent_credits_micro: 1_200_000,
        input_tokens: 900,
        output_tokens: 300,
        web_search_calls: 2,
        code_interpreter_calls: 3,
    };
    let target = key(PeriodType::Daily, Bucket::Total);
    db.provider
        .transaction(move |tx| {
            Box::pin(async move { SqlQuotaLedger.settle_bucket(tx, &target, &delta).await })
        })
        .await
        .unwrap();
    let row = quota_row(&db, "daily", DAY, "total").await.unwrap();
    assert_eq!(row.spent, 26_200_000);
    assert_eq!(row.reserved, 0);
    assert_eq!(row.calls, 1);
    assert_eq!((row.input_tokens, row.output_tokens), (900, 300));
    assert_eq!((row.web_search_calls, row.code_interpreter_calls), (3, 5));
    assert_eq!(
        (
            row.file_search_calls,
            row.image_inputs,
            row.rag_retrieval_calls
        ),
        (0, 0, 0)
    );
    assert_eq!(row.image_upload_bytes, 0);
}

#[tokio::test]
async fn a_zero_delta_still_counts_the_call_and_releases_the_reserve() {
    let db = setup().await;
    seed_rows(
        &db,
        &[row("monthly", MONTH, "tier:premium").reserved(3_750_000)],
    )
    .await;
    let delta = SettleDelta {
        release_reserved_credits_micro: 3_750_000,
        spent_credits_micro: 0,
        input_tokens: 0,
        output_tokens: 0,
        web_search_calls: 0,
        code_interpreter_calls: 0,
    };
    let target = key(PeriodType::Monthly, Bucket::TierPremium);
    db.provider
        .transaction(move |tx| {
            Box::pin(async move { SqlQuotaLedger.settle_bucket(tx, &target, &delta).await })
        })
        .await
        .unwrap();
    let row = quota_row(&db, "monthly", MONTH, "tier:premium")
        .await
        .unwrap();
    assert_eq!((row.spent, row.reserved, row.calls), (0, 0, 1));
}
