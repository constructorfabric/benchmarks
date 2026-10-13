#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Slice 3: the reserve write of `SeaQuotaRepository` on `SQLite`
//! (`cpt-cf-mini-chat-algo-quota-billing-reserve-write`,
//! `cpt-cf-mini-chat-flow-quota-billing-turn-quota-steps`,
//! `cpt-cf-mini-chat-dod-quota-billing-reserve-write`): write-first upserts in
//! the canonical order, the in-transaction re-check, the `chat_turns` fill, the
//! caller's extra steps in the same transaction, the bounded conflict retry and
//! two concurrent reserves on a file-backed WAL database.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use futures::future::join_all;
use sea_orm::ActiveValue::Set;
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};
use time::OffsetDateTime;
use toolkit_db::DbTx;
use toolkit_db::secure::{SecureEntityExt, secure_insert};
use toolkit_security::AccessScope;
use uuid::Uuid;

use crate::domain::error::{DomainError, QuotaSubject};
use crate::domain::service::quota::periods::PeriodKeys;
use crate::domain::service::quota::ports::QuotaRepository;
use crate::domain::service::quota::preflight::PreflightDecision;
use crate::domain::service::quota::reserve::{ReserveMode, ReserveRequest, ReservedColumns};
use crate::domain::service::quota::test_support::{
    date, limits, noon, premium_decision, standard_decision, utc,
};
use crate::infra::db::entities::{chat_turn, quota_usage};
use crate::infra::db::repos::quota::TxFuture;
use crate::quota_test_env::{BucketSeed, NOON_TEXT, QuotaEnv, TurnSeed};
use crate::test_doubles::{LogCapture, OTHER_TENANT, OTHER_USER, USER, USER_TENANT};

const BUSY: &str = "error returned from database: (code: 5) database is locked";

fn request(decision: &PreflightDecision, mode: ReserveMode) -> ReserveRequest<'_> {
    ReserveRequest {
        tenant_id: USER_TENANT,
        user_id: USER,
        mode,
        decision,
        now: noon(),
    }
}

fn expect_tokens_exceeded<T: std::fmt::Debug>(result: &Result<T, DomainError>) {
    assert!(
        matches!(
            result,
            Err(DomainError::QuotaExceeded(QuotaSubject::Tokens))
        ),
        "{result:?}"
    );
}

// ---- the writes ---------------------------------------------------------------------------------------

#[tokio::test]
async fn a_standard_reserve_creates_the_daily_and_monthly_total_rows_only() {
    let env = QuotaEnv::new().await;
    let decision = standard_decision();

    let columns = env
        .repo
        .reserve(&request(&decision, ReserveMode::Send))
        .await
        .unwrap();

    for period in ["daily", "monthly"] {
        let row = env.total(period).await.expect(period);
        assert_eq!(row.reserved, 1_500_000, "{period}");
        assert_eq!(row.spent, 0, "{period}");
        assert_eq!(
            (
                row.calls,
                row.input_tokens,
                row.output_tokens,
                row.web,
                row.ci
            ),
            (0, 0, 0, 0, 0),
            "{period}: every other counter is 0"
        );
        assert_eq!(row.updated_at, NOON_TEXT, "{period}");
        assert!(
            env.premium(period).await.is_none(),
            "{period}: no premium row"
        );
    }
    assert_eq!(env.bucket_count().await, 2);
    assert_eq!(columns, ReservedColumns::from_decision(&decision));
}

#[tokio::test]
async fn a_premium_reserve_also_creates_the_premium_rows() {
    let env = QuotaEnv::new().await;
    let decision = premium_decision();

    env.repo
        .reserve(&request(&decision, ReserveMode::Send))
        .await
        .unwrap();

    for period in ["daily", "monthly"] {
        assert_eq!(
            env.total(period).await.unwrap().reserved,
            3_750_000,
            "{period}"
        );
        let premium = env.premium(period).await.expect(period);
        assert_eq!(premium.reserved, 3_750_000, "{period}");
        assert_eq!(premium.spent, 0, "{period}");
    }
    assert_eq!(env.bucket_count().await, 4);
}

#[tokio::test]
async fn a_new_row_gets_its_own_uuid_id() {
    let env = QuotaEnv::new().await;

    env.repo
        .reserve(&request(&premium_decision(), ReserveMode::Send))
        .await
        .unwrap();

    let distinct = env
        .db
        .query_i64("SELECT COUNT(DISTINCT id) FROM quota_usage WHERE id IS NOT NULL")
        .await;
    assert_eq!(distinct, 4);
}

#[tokio::test]
async fn an_existing_row_gets_its_reserve_increased_and_nothing_else_changed() {
    let env = QuotaEnv::new().await;
    env.seed_bucket(
        &BucketSeed::total("daily", 5_000, 1_000_000)
            .counters(3, 40, 50)
            .tools(1, 2),
    )
    .await;
    env.seed_bucket(&BucketSeed::total("monthly", 9_000, 0))
        .await;

    env.repo
        .reserve(&request(&standard_decision(), ReserveMode::Send))
        .await
        .unwrap();

    let daily = env.total("daily").await.unwrap();
    assert_eq!(daily.reserved, 2_500_000);
    assert_eq!(daily.spent, 5_000);
    assert_eq!(
        (
            daily.calls,
            daily.input_tokens,
            daily.output_tokens,
            daily.web,
            daily.ci
        ),
        (3, 40, 50, 1, 2)
    );
    assert_eq!(daily.updated_at, NOON_TEXT);
    let monthly = env.total("monthly").await.unwrap();
    assert_eq!((monthly.reserved, monthly.spent), (1_500_000, 9_000));
    assert_eq!(env.bucket_count().await, 2, "no duplicate row");
}

#[tokio::test]
async fn two_reserves_accumulate_in_the_same_row() {
    let env = QuotaEnv::new().await;
    let decision = standard_decision();

    env.repo
        .reserve(&request(&decision, ReserveMode::Send))
        .await
        .unwrap();
    env.repo
        .reserve(&request(&decision, ReserveMode::Send))
        .await
        .unwrap();

    assert_eq!(env.total("daily").await.unwrap().reserved, 3_000_000);
    assert_eq!(env.bucket_count().await, 2);
}

#[tokio::test]
async fn the_rows_are_keyed_by_the_preflight_period_starts_not_by_the_clock() {
    let env = QuotaEnv::new().await;
    let mut decision = standard_decision();
    decision.period_keys = PeriodKeys {
        daily_start: date(2026, 3, 9),
        monthly_start: date(2026, 3, 1),
    };
    // the clock is already on the next day
    let mut req = request(&decision, ReserveMode::Send);
    req.now = utc(2026, 3, 10, 0, 0, 5);

    env.repo.reserve(&req).await.unwrap();

    assert!(
        env.bucket_of(USER_TENANT, USER, "daily", "2026-03-09", "total")
            .await
            .is_some()
    );
    assert!(env.total("daily").await.is_none(), "nothing on 2026-03-10");
}

#[tokio::test]
async fn only_the_rows_of_the_turns_tenant_and_user_are_touched() {
    let env = QuotaEnv::new().await;
    env.seed_bucket(&BucketSeed::total("daily", 7, 7).of(USER_TENANT, OTHER_USER))
        .await;
    env.seed_bucket(&BucketSeed::total("daily", 8, 8).of(OTHER_TENANT, USER))
        .await;

    env.repo
        .reserve(&request(&standard_decision(), ReserveMode::Send))
        .await
        .unwrap();

    let other_user = env
        .bucket_of(USER_TENANT, OTHER_USER, "daily", "2026-03-10", "total")
        .await
        .unwrap();
    let other_tenant = env
        .bucket_of(OTHER_TENANT, USER, "daily", "2026-03-10", "total")
        .await
        .unwrap();
    assert_eq!((other_user.spent, other_user.reserved), (7, 7));
    assert_eq!((other_tenant.spent, other_tenant.reserved), (8, 8));
    assert_eq!(env.total("daily").await.unwrap().reserved, 1_500_000);
}

#[tokio::test]
async fn send_mode_writes_no_turn_row() {
    let env = QuotaEnv::new().await;
    env.seed_chat(1).await;

    env.repo
        .reserve(&request(&standard_decision(), ReserveMode::Send))
        .await
        .unwrap();

    assert_eq!(env.db.count("chat_turns").await, 0);
}

// ---- the re-check -----------------------------------------------------------------------------------

#[tokio::test]
async fn a_reserve_that_no_longer_fits_at_the_recheck_is_a_tokens_429_and_writes_nothing() {
    let env = QuotaEnv::new().await;
    // another in-flight turn already holds 59_000_000 of the 60_000_000 daily limit
    env.seed_bucket(&BucketSeed::total("daily", 0, 59_000_000))
        .await;
    let before = env.quota_dump().await;

    let result = env
        .repo
        .reserve(&request(&standard_decision(), ReserveMode::Send))
        .await;

    expect_tokens_exceeded(&result);
    assert_eq!(
        env.quota_dump().await,
        before,
        "the increments were rolled back"
    );
}

#[tokio::test]
async fn the_recheck_passes_exactly_at_the_limit_and_counts_spent_credits() {
    let env = QuotaEnv::new().await;
    env.seed_bucket(&BucketSeed::total("daily", 40_000_000, 18_500_000))
        .await;

    env.repo
        .reserve(&request(&standard_decision(), ReserveMode::Send))
        .await
        .unwrap();

    let daily = env.total("daily").await.unwrap();
    assert_eq!(
        daily.spent + daily.reserved,
        60_000_000,
        "exactly at the limit"
    );
}

#[tokio::test]
async fn one_micro_credit_above_the_limit_fails_the_recheck() {
    let env = QuotaEnv::new().await;
    env.seed_bucket(&BucketSeed::total("daily", 40_000_000, 18_500_001))
        .await;

    let result = env
        .repo
        .reserve(&request(&standard_decision(), ReserveMode::Send))
        .await;

    expect_tokens_exceeded(&result);
}

#[tokio::test]
async fn an_over_limit_monthly_bucket_rolls_back_the_daily_increment_too() {
    let env = QuotaEnv::new().await;
    env.seed_bucket(&BucketSeed::total("daily", 1, 0)).await;
    env.seed_bucket(&BucketSeed::total("monthly", 599_000_000, 0))
        .await;
    let before = env.quota_dump().await;

    let result = env
        .repo
        .reserve(&request(&standard_decision(), ReserveMode::Send))
        .await;

    expect_tokens_exceeded(&result);
    assert_eq!(env.quota_dump().await, before);
}

#[tokio::test]
async fn an_over_limit_premium_bucket_is_a_429_even_when_the_totals_fit_and_the_turn_is_not_downgraded()
 {
    let env = QuotaEnv::new().await;
    // premium daily limit 22_000_000: another premium turn holds 20_000_000
    env.seed_bucket(&BucketSeed::premium("daily", 0, 20_000_000))
        .await;
    let before = env.quota_dump().await;

    let result = env
        .repo
        .reserve(&request(&premium_decision(), ReserveMode::Send))
        .await;

    expect_tokens_exceeded(&result);
    assert_eq!(
        env.quota_dump().await,
        before,
        "no total row was left behind either"
    );
}

#[tokio::test]
async fn the_recheck_uses_the_limits_of_the_decision() {
    let env = QuotaEnv::new().await;
    let mut decision = standard_decision();
    decision.limits = limits(7, 1_499_999, 600_000_000, 22_000_000, 300_000_000);

    let result = env
        .repo
        .reserve(&request(&decision, ReserveMode::Send))
        .await;

    expect_tokens_exceeded(&result);

    decision.limits = limits(7, 1_500_000, 600_000_000, 22_000_000, 300_000_000);
    env.repo
        .reserve(&request(&decision, ReserveMode::Send))
        .await
        .unwrap();
}

// ---- fill mode --------------------------------------------------------------------------------------

#[tokio::test]
async fn fill_mode_writes_the_six_columns_of_the_new_turn_with_the_increments() {
    let env = QuotaEnv::new().await;
    let chat = env.seed_chat(1).await;
    let turn = env
        .seed_turn(&TurnSeed::standard(1, chat).without_reserve())
        .await;
    let decision = standard_decision();

    let columns = env
        .repo
        .reserve(&request(&decision, ReserveMode::Fill { turn_id: turn }))
        .await
        .unwrap();

    assert_eq!(columns, ReservedColumns::from_decision(&decision));
    assert_eq!(env.turn_col(turn, "reserve_tokens").await, "1500");
    assert_eq!(env.turn_col(turn, "max_output_tokens_applied").await, "500");
    assert_eq!(
        env.turn_col(turn, "reserved_credits_micro").await,
        "1500000"
    );
    assert_eq!(env.turn_col(turn, "policy_version_applied").await, "7");
    assert_eq!(env.turn_col(turn, "effective_model").await, "'s'");
    assert_eq!(
        env.turn_col(turn, "minimal_generation_floor_applied").await,
        "50"
    );
    assert_eq!(env.turn_state(turn).await, "running");
    assert_eq!(env.total("daily").await.unwrap().reserved, 1_500_000);
    assert_eq!(env.total("monthly").await.unwrap().reserved, 1_500_000);
}

#[tokio::test]
async fn the_columns_are_written_once_a_second_fill_is_an_internal_error_and_rolls_back() {
    let env = QuotaEnv::new().await;
    let chat = env.seed_chat(1).await;
    let turn = env
        .seed_turn(&TurnSeed::standard(1, chat).without_reserve())
        .await;
    let decision = standard_decision();
    env.repo
        .reserve(&request(&decision, ReserveMode::Fill { turn_id: turn }))
        .await
        .unwrap();
    let before = env.quota_dump().await;

    let result = env
        .repo
        .reserve(&request(&decision, ReserveMode::Fill { turn_id: turn }))
        .await;

    assert!(
        matches!(result, Err(DomainError::Internal(_))),
        "{result:?}"
    );
    assert_eq!(
        env.quota_dump().await,
        before,
        "the second increment is rolled back"
    );
    assert_eq!(
        env.turn_col(turn, "reserved_credits_micro").await,
        "1500000"
    );
}

#[tokio::test]
async fn a_fill_of_a_turn_that_is_not_running_or_missing_is_internal_and_reserves_nothing() {
    let env = QuotaEnv::new().await;
    // control: a running turn is filled
    let own_chat = env.seed_chat(2).await;
    let own_turn = env
        .seed_turn(&TurnSeed::standard(2, own_chat).without_reserve())
        .await;
    env.repo
        .reserve(&request(
            &standard_decision(),
            ReserveMode::Fill { turn_id: own_turn },
        ))
        .await
        .unwrap();
    let chat = env.seed_chat(1).await;
    let terminal = env
        .seed_turn(
            &TurnSeed::standard(1, chat)
                .without_reserve()
                .state("completed"),
        )
        .await;
    let decision = standard_decision();

    for turn in [terminal, Uuid::from_u128(0xDEAD)] {
        let result = env
            .repo
            .reserve(&request(&decision, ReserveMode::Fill { turn_id: turn }))
            .await;

        assert!(
            matches!(result, Err(DomainError::Internal(_))),
            "{turn}: {result:?}"
        );
    }
    assert_eq!(
        env.bucket_count().await,
        2,
        "every failed attempt rolled back, only the control reserve is booked"
    );
    assert_eq!(env.turn_col(terminal, "reserve_tokens").await, "NULL");
}

#[tokio::test]
async fn a_fill_never_touches_a_turn_of_another_tenant() {
    let env = QuotaEnv::new().await;
    // control: a turn of the requesting tenant is filled
    let own_chat = env.seed_chat(2).await;
    let own_turn = env
        .seed_turn(&TurnSeed::standard(2, own_chat).without_reserve())
        .await;
    env.repo
        .reserve(&request(
            &standard_decision(),
            ReserveMode::Fill { turn_id: own_turn },
        ))
        .await
        .unwrap();
    assert_eq!(env.turn_col(own_turn, "reserve_tokens").await, "1500");
    let chat = env.seed_chat_of(1, OTHER_TENANT, OTHER_USER).await;
    let mut seed = TurnSeed::standard(1, chat).without_reserve();
    seed.tenant = OTHER_TENANT;
    let turn = env.seed_turn(&seed).await;

    let result = env
        .repo
        .reserve(&request(
            &standard_decision(),
            ReserveMode::Fill { turn_id: turn },
        ))
        .await;

    assert!(
        matches!(result, Err(DomainError::Internal(_))),
        "{result:?}"
    );
    assert_eq!(env.turn_col(turn, "reserve_tokens").await, "NULL");
    assert_eq!(
        env.bucket_count().await,
        2,
        "only the control reserve is booked"
    );
}

// ---- the caller's steps in the same transaction -----------------------------------------------------------

fn insert_running_turn<'a>(
    tx: &'a DbTx<'a>,
    chat: Uuid,
    turn: Uuid,
    reserved: &'a ReservedColumns,
    now: OffsetDateTime,
) -> TxFuture<'a> {
    Box::pin(async move {
        let row = chat_turn::ActiveModel {
            id: Set(turn),
            tenant_id: Set(USER_TENANT),
            chat_id: Set(chat),
            request_id: Set(Uuid::from_u128(0xE5)),
            requester_type: Set("user".to_owned()),
            requester_user_id: Set(Some(USER)),
            state: Set("running".to_owned()),
            provider_name: Set(None),
            provider_response_id: Set(None),
            assistant_message_id: Set(None),
            error_code: Set(None),
            reserve_tokens: Set(Some(reserved.reserve_tokens)),
            max_output_tokens_applied: Set(Some(
                i32::try_from(reserved.max_output_tokens_applied).unwrap(),
            )),
            reserved_credits_micro: Set(Some(reserved.reserved_credits_micro)),
            policy_version_applied: Set(Some(
                i64::try_from(reserved.policy_version_applied).unwrap(),
            )),
            effective_model: Set(Some(reserved.effective_model.clone())),
            minimal_generation_floor_applied: Set(Some(
                i32::try_from(reserved.minimal_generation_floor_applied).unwrap(),
            )),
            error_detail: Set(None),
            deleted_at: Set(None),
            replaced_by_request_id: Set(None),
            started_at: Set(now),
            last_progress_at: Set(Some(now)),
            web_search_enabled: Set(false),
            web_search_completed_count: Set(0),
            code_interpreter_completed_count: Set(0),
            file_search_completed_count: Set(0),
            completed_at: Set(None),
            updated_at: Set(now),
        };
        secure_insert::<chat_turn::Entity>(row, &AccessScope::for_tenant(USER_TENANT), tx)
            .await
            .map(|_| ())
            .map_err(|error| DomainError::Database(error.to_string()))
    })
}

#[tokio::test]
async fn the_send_path_inserts_the_turn_row_in_the_same_transaction_as_the_increments() {
    let env = QuotaEnv::new().await;
    let chat = env.seed_chat(1).await;
    let turn = Uuid::from_u128(0xA1);
    let decision = standard_decision();
    let seen = Arc::new(AtomicU32::new(0));
    let seen_in = Arc::clone(&seen);

    let columns = env
        .repo
        .reserve_with(
            &request(&decision, ReserveMode::Send),
            move |tx, reserved| {
                seen_in.fetch_add(1, Ordering::SeqCst);
                insert_running_turn(tx, chat, turn, reserved, noon())
            },
        )
        .await
        .unwrap();

    assert_eq!(seen.load(Ordering::SeqCst), 1);
    assert_eq!(env.turn_state(turn).await, "running");
    assert_eq!(env.turn_col(turn, "reserve_tokens").await, "1500");
    assert_eq!(
        env.turn_col(turn, "reserved_credits_micro").await,
        columns.reserved_credits_micro.to_string()
    );
    assert_eq!(env.total("daily").await.unwrap().reserved, 1_500_000);
    assert_eq!(env.total("monthly").await.unwrap().reserved, 1_500_000);
}

#[tokio::test]
async fn a_failing_caller_step_rolls_the_increments_back() {
    let env = QuotaEnv::new().await;
    let chat = env.seed_chat(1).await;
    let before = env.quota_dump().await;

    let result = env
        .repo
        .reserve_with(
            &request(&standard_decision(), ReserveMode::Send),
            move |tx, reserved| {
                Box::pin(async move {
                    insert_running_turn(tx, chat, Uuid::from_u128(0xA1), reserved, noon()).await?;
                    Err(DomainError::Internal(
                        "the message insert failed".to_owned(),
                    ))
                })
            },
        )
        .await;

    assert!(
        matches!(&result, Err(DomainError::Internal(text)) if text.contains("message insert")),
        "{result:?}"
    );
    assert_eq!(env.quota_dump().await, before);
    assert_eq!(
        env.db.count("chat_turns").await,
        0,
        "no turn row is left behind"
    );
}

#[tokio::test]
async fn the_caller_step_runs_after_the_increments_and_sees_them_in_its_transaction() {
    let env = QuotaEnv::new().await;
    let observed = Arc::new(std::sync::Mutex::new(Vec::new()));
    let observed_in = Arc::clone(&observed);

    env.repo
        .reserve_with(
            &request(&standard_decision(), ReserveMode::Send),
            move |tx, _reserved| {
                let observed = Arc::clone(&observed_in);
                Box::pin(async move {
                    let rows = quota_usage::Entity::find()
                        .filter(quota_usage::Column::Bucket.eq("total"))
                        .secure()
                        .scope_with(&AccessScope::for_tenant(USER_TENANT).ensure_owner(USER))
                        .all(tx)
                        .await
                        .map_err(|e| DomainError::Database(e.to_string()))?;
                    let mut reserved: Vec<i64> =
                        rows.iter().map(|r| r.reserved_credits_micro).collect();
                    reserved.sort_unstable();
                    observed.lock().unwrap().push(reserved);
                    Ok(())
                })
            },
        )
        .await
        .unwrap();

    assert_eq!(*observed.lock().unwrap(), vec![vec![1_500_000, 1_500_000]]);
}

#[tokio::test]
async fn the_caller_step_is_not_run_when_the_recheck_fails() {
    let env = QuotaEnv::new().await;
    env.seed_bucket(&BucketSeed::total("daily", 0, 59_500_000))
        .await;
    let calls = Arc::new(AtomicU32::new(0));
    let calls_in = Arc::clone(&calls);

    let result = env
        .repo
        .reserve_with(
            &request(&standard_decision(), ReserveMode::Send),
            move |_tx, _r| {
                calls_in.fetch_add(1, Ordering::SeqCst);
                Box::pin(async { Ok(()) })
            },
        )
        .await;

    expect_tokens_exceeded(&result);
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

// ---- the bounded conflict retry ----------------------------------------------------------------------

/// A caller step that fails with a lock conflict for the first `conflicts`
/// attempts.
fn conflicting(
    conflicts: u32,
    attempts: &Arc<AtomicU32>,
) -> impl for<'a> Fn(&'a DbTx<'a>, &'a ReservedColumns) -> TxFuture<'a> + Send + Sync + use<> {
    let attempts = Arc::clone(attempts);
    move |_tx, _reserved| {
        let attempt = attempts.fetch_add(1, Ordering::SeqCst) + 1;
        Box::pin(async move {
            if attempt <= conflicts {
                Err(DomainError::Database(BUSY.to_owned()))
            } else {
                Ok(())
            }
        })
    }
}

#[tokio::test]
async fn two_conflicts_are_retried_and_the_third_attempt_reserves_exactly_once() {
    let env = QuotaEnv::new().await;
    let attempts = Arc::new(AtomicU32::new(0));

    let result = env
        .repo
        .reserve_with(
            &request(&standard_decision(), ReserveMode::Send),
            conflicting(2, &attempts),
        )
        .await;

    assert!(result.is_ok(), "{result:?}");
    assert_eq!(attempts.load(Ordering::SeqCst), 3);
    assert_eq!(
        env.total("daily").await.unwrap().reserved,
        1_500_000,
        "the rolled-back attempts left no increment"
    );
    assert_eq!(env.total("monthly").await.unwrap().reserved, 1_500_000);
}

#[tokio::test]
async fn a_conflict_that_persists_through_three_attempts_is_a_database_error_with_nothing_reserved()
{
    let env = QuotaEnv::new().await;
    let attempts = Arc::new(AtomicU32::new(0));

    let result = env
        .repo
        .reserve_with(
            &request(&standard_decision(), ReserveMode::Send),
            conflicting(u32::MAX, &attempts),
        )
        .await;

    assert!(
        matches!(result, Err(DomainError::Database(_))),
        "{result:?}"
    );
    assert_eq!(
        attempts.load(Ordering::SeqCst),
        3,
        "three attempts in total"
    );
    assert_eq!(env.bucket_count().await, 0);
}

#[tokio::test]
async fn the_snapshot_busy_code_517_is_a_conflict_too() {
    let env = QuotaEnv::new().await;
    let attempts = Arc::new(AtomicU32::new(0));
    let attempts_in = Arc::clone(&attempts);

    let result = env
        .repo
        .reserve_with(
            &request(&standard_decision(), ReserveMode::Send),
            move |_tx, _r| {
                let attempt = attempts_in.fetch_add(1, Ordering::SeqCst);
                Box::pin(async move {
                    if attempt == 0 {
                        Err(DomainError::Database(
                            "error returned from database: (code: 517) database is locked"
                                .to_owned(),
                        ))
                    } else {
                        Ok(())
                    }
                })
            },
        )
        .await;

    assert!(result.is_ok(), "{result:?}");
    assert_eq!(attempts.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn an_error_that_is_not_a_conflict_is_not_retried() {
    let env = QuotaEnv::new().await;
    let attempts = Arc::new(AtomicU32::new(0));
    let attempts_in = Arc::clone(&attempts);

    let result = env
        .repo
        .reserve_with(
            &request(&standard_decision(), ReserveMode::Send),
            move |_tx, _r| {
                attempts_in.fetch_add(1, Ordering::SeqCst);
                Box::pin(async {
                    Err(DomainError::Database("UNIQUE constraint failed".to_owned()))
                })
            },
        )
        .await;

    assert!(matches!(result, Err(DomainError::Database(_))));
    assert_eq!(attempts.load(Ordering::SeqCst), 1);
    assert_eq!(env.bucket_count().await, 0);
}

#[tokio::test]
async fn a_429_is_never_retried_and_a_conflict_is_never_a_429() {
    let env = QuotaEnv::new().await;
    env.seed_bucket(&BucketSeed::total("daily", 0, 59_500_000))
        .await;
    let attempts = Arc::new(AtomicU32::new(0));

    let rejected = env
        .repo
        .reserve_with(
            &request(&standard_decision(), ReserveMode::Send),
            conflicting(0, &attempts),
        )
        .await;
    expect_tokens_exceeded(&rejected);
    assert_eq!(attempts.load(Ordering::SeqCst), 0);

    let exhausted_env = QuotaEnv::new().await;
    let attempts = Arc::new(AtomicU32::new(0));
    let conflicted = exhausted_env
        .repo
        .reserve_with(
            &request(&standard_decision(), ReserveMode::Send),
            conflicting(u32::MAX, &attempts),
        )
        .await;
    assert!(
        !matches!(conflicted, Err(DomainError::QuotaExceeded(_))),
        "{conflicted:?}"
    );
}

// ---- concurrency on a file-backed WAL database ---------------------------------------------------------

fn tight_decision(total_daily_limit: i64) -> PreflightDecision {
    let mut decision = standard_decision();
    decision.limits = limits(7, total_daily_limit, 600_000_000, 22_000_000, 300_000_000);
    decision
}

fn is_clean_outcome<T: std::fmt::Debug>(result: &Result<T, DomainError>) -> bool {
    matches!(
        result,
        Ok(_) | Err(DomainError::QuotaExceeded(QuotaSubject::Tokens))
    )
}

#[tokio::test]
async fn two_concurrent_reserves_that_together_exceed_the_limit_give_one_success_and_one_429() {
    let env = QuotaEnv::file().await;
    // 2_000_000 daily: one 1_500_000 reserve fits, two do not
    let decision = tight_decision(2_000_000);

    for round in 0..8_u128 {
        let user = Uuid::from_u128(0x1000 + round);
        let make = |user| ReserveRequest {
            tenant_id: USER_TENANT,
            user_id: user,
            mode: ReserveMode::Send,
            decision: &decision,
            now: noon(),
        };
        let (first_req, second_req) = (make(user), make(user));

        let (first, second) =
            tokio::join!(env.repo.reserve(&first_req), env.repo.reserve(&second_req));

        assert!(is_clean_outcome(&first), "round {round}: {first:?}");
        assert!(is_clean_outcome(&second), "round {round}: {second:?}");
        assert_eq!(
            usize::from(first.is_ok()) + usize::from(second.is_ok()),
            1,
            "round {round}: {first:?} / {second:?}"
        );
        let row = env
            .bucket_of(USER_TENANT, user, "daily", "2026-03-10", "total")
            .await
            .unwrap();
        assert_eq!(
            row.reserved, 1_500_000,
            "round {round}: exactly one reserve is booked"
        );
    }
}

#[tokio::test]
async fn many_concurrent_reserves_book_exactly_as_many_as_the_limit_allows() {
    let env = QuotaEnv::file().await;
    // 3_100_000 daily: two 1_500_000 reserves fit, three do not
    let decision = tight_decision(3_100_000);
    let requests: Vec<_> = (0..6)
        .map(|_| ReserveRequest {
            tenant_id: USER_TENANT,
            user_id: USER,
            mode: ReserveMode::Send,
            decision: &decision,
            now: noon(),
        })
        .collect();

    let results = join_all(requests.iter().map(|r| env.repo.reserve(r))).await;

    for result in &results {
        assert!(is_clean_outcome(result), "{result:?}");
    }
    assert_eq!(
        results.iter().filter(|r| r.is_ok()).count(),
        2,
        "{results:?}"
    );
    assert_eq!(env.total("daily").await.unwrap().reserved, 3_000_000);
    assert_eq!(env.total("monthly").await.unwrap().reserved, 3_000_000);
}

#[tokio::test]
async fn concurrent_reserves_of_different_users_all_succeed() {
    let env = QuotaEnv::file().await;
    let decision = standard_decision();
    let requests: Vec<_> = (0..6_u128)
        .map(|n| ReserveRequest {
            tenant_id: USER_TENANT,
            user_id: Uuid::from_u128(0x2000 + n),
            mode: ReserveMode::Send,
            decision: &decision,
            now: noon(),
        })
        .collect();

    let results = join_all(requests.iter().map(|r| env.repo.reserve(r))).await;

    for result in &results {
        assert!(result.is_ok(), "{result:?}");
    }
    assert_eq!(env.bucket_count().await, 12);
}

#[tokio::test]
async fn concurrent_premium_reserves_respect_the_premium_limit_too() {
    let env = QuotaEnv::file().await;
    let mut decision = premium_decision();
    // premium daily 5_000_000: one 3_750_000 reserve fits, two do not
    decision.limits = limits(7, 600_000_000, 600_000_000, 5_000_000, 300_000_000);
    let requests: Vec<_> = (0..4)
        .map(|_| ReserveRequest {
            tenant_id: USER_TENANT,
            user_id: USER,
            mode: ReserveMode::Send,
            decision: &decision,
            now: noon(),
        })
        .collect();

    let results = join_all(requests.iter().map(|r| env.repo.reserve(r))).await;

    for result in &results {
        assert!(is_clean_outcome(result), "{result:?}");
    }
    assert_eq!(
        results.iter().filter(|r| r.is_ok()).count(),
        1,
        "{results:?}"
    );
    assert_eq!(env.premium("daily").await.unwrap().reserved, 3_750_000);
    assert_eq!(env.total("daily").await.unwrap().reserved, 3_750_000);
}

#[tokio::test]
async fn the_rejected_reserves_leave_no_log_of_a_database_error() {
    let capture = LogCapture::start();
    let env = QuotaEnv::file().await;
    let decision = tight_decision(2_000_000);
    let (first_req, second_req) = (
        request(&decision, ReserveMode::Send),
        request(&decision, ReserveMode::Send),
    );

    let (first, second) = tokio::join!(env.repo.reserve(&first_req), env.repo.reserve(&second_req));

    assert_eq!(usize::from(first.is_ok()) + usize::from(second.is_ok()), 1);
    assert!(
        !capture.all_text().contains("database is locked"),
        "{}",
        capture.all_text()
    );
}

// ---- write-first -------------------------------------------------------------------------------------

/// Holds the `SQLite` write lock on the raw connection for `hold`, then
/// releases it.
async fn hold_write_lock(env: &QuotaEnv, hold: std::time::Duration) {
    use sea_orm::ConnectionTrait;
    env.db
        .raw
        .execute_unprepared("BEGIN IMMEDIATE")
        .await
        .unwrap();
    env.db
        .raw
        .execute_unprepared("UPDATE chats SET updated_at = updated_at")
        .await
        .unwrap();
    tokio::time::sleep(hold).await;
    env.db.raw.execute_unprepared("COMMIT").await.unwrap();
}

#[tokio::test]
async fn a_reserve_waits_for_a_competing_writer_instead_of_failing_because_its_first_statement_is_a_write()
 {
    let env = QuotaEnv::file().await;
    env.seed_chat(1).await;
    let decision = standard_decision();
    let req = request(&decision, ReserveMode::Send);

    // The competing writer holds the lock for 400 ms, longer than the whole
    // conflict backoff (20 ms + 100 ms): only a transaction that starts with
    // its write and waits for the lock (busy timeout) gets through; a
    // transaction that reads first cannot upgrade to a write lock and would
    // exhaust its retries.
    let (result, ()) = tokio::join!(
        async {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            env.repo.reserve(&req).await
        },
        hold_write_lock(&env, std::time::Duration::from_millis(400))
    );

    assert!(result.is_ok(), "{result:?}");
    assert_eq!(env.total("daily").await.unwrap().reserved, 1_500_000);
}
