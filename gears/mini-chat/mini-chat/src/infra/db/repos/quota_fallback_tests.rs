#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Slice 4: the deterministic settlement fallback of the finalization
//! (`cpt-cf-mini-chat-algo-quota-billing-settle`, steps 2, 5, 6 and 7, and the
//! feature-local deviation "Deterministic settlement fallback"): an unavailable
//! snapshot, an effective model missing from it, a credit error. The CAS winner
//! never rolls back: it settles `estimated` with `reserved_credits_micro`.

use mini_chat_sdk::{BillingOutcome, ModelTier, SettlementMethod, TerminalState};
use tracing::Level;
use uuid::Uuid;

use crate::domain::service::quota::arith::CreditError;
use crate::domain::service::quota::finalize::{FinalizeRequest, FinalizeResult, FinalizeWon};
use crate::domain::service::quota::policy::SettlementSnapshot;
use crate::domain::service::quota::ports::QuotaRepository;
use crate::domain::service::quota::settle::FallbackCause;
use crate::domain::service::quota::test_support::{
    PolicyCall, PolicyStep, model, noon, settlement_snapshot, snapshot, utc,
};
use crate::quota_test_env::{BucketSeed, QuotaEnv, StreamReq, TurnSeed};
use crate::test_doubles::{LogCapture, USER};

async fn standard_world() -> (QuotaEnv, Uuid) {
    let env = QuotaEnv::new().await;
    let chat = env.seed_chat(1).await;
    let turn = env.seed_turn(&TurnSeed::standard(1, chat)).await;
    env.seed_design_world(1_500_000, false).await;
    (env, turn)
}

async fn premium_world() -> (QuotaEnv, Uuid) {
    let env = QuotaEnv::new().await;
    let chat = env.seed_chat(1).await;
    let turn = env.seed_turn(&TurnSeed::premium(1, chat)).await;
    env.seed_design_world(3_750_000, true).await;
    (env, turn)
}

fn won(result: FinalizeResult) -> FinalizeWon {
    match result {
        FinalizeResult::Won(won) => *won,
        FinalizeResult::Lost => panic!("the CAS was lost"),
    }
}

async fn finalize(env: &QuotaEnv, request: &StreamReq) -> FinalizeWon {
    won(env.repo.finalize_turn(&request.build()).await.unwrap())
}

async fn event_of(env: &QuotaEnv) -> (mini_chat_sdk::UsageEvent, serde_json::Value) {
    let rows = env.usage_rows().await;
    assert_eq!(rows.len(), 1, "exactly one usage message");
    (
        mini_chat_sdk::decode_usage_event(&rows[0].payload).unwrap(),
        rows[0].json(),
    )
}

// ---- an unavailable snapshot -------------------------------------------------------------------------

#[tokio::test]
async fn an_unavailable_snapshot_still_finalizes_the_turn_estimated_at_the_reserved_credits() {
    let capture = LogCapture::start();
    let (env, turn) = standard_world().await;

    let won = finalize(
        &env,
        &StreamReq::completed(turn)
            .usage(900, 300)
            .tools(1, 2, 0)
            .snapshot(SettlementSnapshot::Unavailable),
    )
    .await;

    assert_eq!(
        won.terminal_state,
        TerminalState::Completed,
        "the turn is finalized as it ended"
    );
    assert_eq!(won.billing_outcome, BillingOutcome::Completed);
    assert_eq!(won.settlement_method, SettlementMethod::Estimated);
    assert_eq!(won.committed_credits_micro, 1_500_000);
    assert_eq!(won.fallback, Some(FallbackCause::SnapshotUnavailable));
    assert!(won.settled && won.usage_event_enqueued);
    assert_eq!((won.actual_input_tokens, won.actual_output_tokens), (0, 0));
    assert_eq!(env.turn_state(turn).await, "completed");
    for period in ["daily", "monthly"] {
        let total = env.total(period).await.unwrap();
        let spent_before = if period == "daily" {
            25_000_000
        } else {
            240_000_000
        };
        assert_eq!(total.spent, spent_before + 1_500_000, "{period}");
        assert_eq!(total.reserved, 0, "{period}: back to the prior value");
        assert_eq!(total.calls, 1, "{period}");
        assert_eq!(
            (total.input_tokens, total.output_tokens),
            (0, 0),
            "{period}"
        );
        assert_eq!(
            (total.web, total.ci),
            (1, 2),
            "{period}: the tool calls are counted"
        );
        let premium = env.premium(period).await.unwrap();
        assert_eq!(
            (premium.reserved, premium.calls),
            (0, 0),
            "{period}: a standard turn"
        );
    }
    let (event, raw) = event_of(&env).await;
    assert_eq!(event.settlement_method, SettlementMethod::Estimated);
    assert_eq!(event.actual_credits_micro, 1_500_000);
    assert_eq!(event.billing_outcome, BillingOutcome::Completed);
    assert!(raw["usage"].is_null());
    let logged = capture.at_containing(Level::ERROR, &turn.to_string());
    assert_eq!(
        logged.len(),
        1,
        "one error log names the cause: {}",
        capture.all_text()
    );
}

#[tokio::test]
async fn the_fallback_commits_the_reserved_credits_even_for_a_client_disconnect() {
    let (env, turn) = standard_world().await;

    let won = finalize(
        &env,
        &StreamReq::cancelled(turn).snapshot(SettlementSnapshot::Unavailable),
    )
    .await;

    assert_eq!(won.billing_outcome, BillingOutcome::Aborted);
    assert_eq!(won.settlement_method, SettlementMethod::Estimated);
    assert_eq!(
        won.committed_credits_micro, 1_500_000,
        "not the 1_050_000 of the multiplier-based estimate"
    );
}

#[tokio::test]
async fn a_premium_turn_is_detected_from_its_persisted_premium_reserve_rows() {
    let (env, turn) = premium_world().await;

    let won = finalize(
        &env,
        &StreamReq::completed(turn)
            .usage(900, 300)
            .snapshot(SettlementSnapshot::Unavailable),
    )
    .await;

    assert_eq!(won.committed_credits_micro, 3_750_000);
    for (period, premium_spent) in [("daily", 23_750_000), ("monthly", 203_750_000)] {
        let premium = env.premium(period).await.unwrap();
        assert_eq!(
            (premium.spent, premium.reserved, premium.calls),
            (premium_spent, 0, 1),
            "{period}"
        );
        assert_eq!(
            (
                premium.input_tokens,
                premium.output_tokens,
                premium.web,
                premium.ci
            ),
            (0, 0, 0, 0)
        );
        let total = env.total(period).await.unwrap();
        assert_eq!((total.reserved, total.calls), (0, 1), "{period}");
    }
}

#[tokio::test]
async fn a_turn_is_premium_only_when_both_premium_rows_hold_at_least_its_reserve() {
    // one period short: the monthly premium row holds less than the turn reserve
    let env = QuotaEnv::new().await;
    let chat = env.seed_chat(1).await;
    let turn = env.seed_turn(&TurnSeed::standard(1, chat)).await;
    env.seed_bucket(&BucketSeed::total("daily", 0, 1_500_000))
        .await;
    env.seed_bucket(&BucketSeed::total("monthly", 0, 1_500_000))
        .await;
    env.seed_bucket(&BucketSeed::premium("daily", 0, 2_000_000))
        .await;
    env.seed_bucket(&BucketSeed::premium("monthly", 0, 1_499_999))
        .await;

    finalize(
        &env,
        &StreamReq::completed(turn).snapshot(SettlementSnapshot::Unavailable),
    )
    .await;

    assert_eq!(env.premium("daily").await.unwrap().calls, 0);
    assert_eq!(env.premium("monthly").await.unwrap().calls, 0);

    // both rows hold at least the reserve (other in-flight premium turns can
    // cover it): the turn is taken for premium
    let env = QuotaEnv::new().await;
    let chat = env.seed_chat(1).await;
    let turn = env.seed_turn(&TurnSeed::standard(1, chat)).await;
    env.seed_bucket(&BucketSeed::total("daily", 0, 1_500_000))
        .await;
    env.seed_bucket(&BucketSeed::total("monthly", 0, 1_500_000))
        .await;
    env.seed_bucket(&BucketSeed::premium("daily", 0, 1_500_000))
        .await;
    env.seed_bucket(&BucketSeed::premium("monthly", 0, 2_000_000))
        .await;

    finalize(
        &env,
        &StreamReq::completed(turn).snapshot(SettlementSnapshot::Unavailable),
    )
    .await;

    assert_eq!(env.premium("daily").await.unwrap().calls, 1);
    assert_eq!(env.premium("monthly").await.unwrap().calls, 1);
}

#[tokio::test]
async fn a_missing_premium_row_means_a_standard_turn() {
    let env = QuotaEnv::new().await;
    let chat = env.seed_chat(1).await;
    let turn = env.seed_turn(&TurnSeed::premium(1, chat)).await;
    env.seed_bucket(&BucketSeed::total("daily", 0, 3_750_000))
        .await;
    env.seed_bucket(&BucketSeed::total("monthly", 0, 3_750_000))
        .await;
    env.seed_bucket(&BucketSeed::premium("daily", 0, 3_750_000))
        .await;
    // no monthly premium row

    finalize(
        &env,
        &StreamReq::completed(turn).snapshot(SettlementSnapshot::Unavailable),
    )
    .await;

    assert_eq!(env.premium("daily").await.unwrap().calls, 0);
    assert!(env.premium("monthly").await.is_none());
}

// ---- the effective model missing, a snapshot of another version --------------------------------------

#[tokio::test]
async fn an_effective_model_missing_from_the_snapshot_takes_the_same_fallback() {
    let (env, turn) = standard_world().await;
    let without_s = settlement_snapshot(
        7,
        vec![
            model("p", ModelTier::Premium)
                .mult(2_500_000_000, 2_500_000_000)
                .build(),
        ],
    );

    let won = finalize(
        &env,
        &StreamReq::completed(turn)
            .usage(900, 300)
            .snapshot(without_s),
    )
    .await;

    assert_eq!(won.settlement_method, SettlementMethod::Estimated);
    assert_eq!(won.committed_credits_micro, 1_500_000);
    assert_eq!(won.fallback, Some(FallbackCause::ModelMissing));
    assert_eq!(env.total("daily").await.unwrap().spent, 26_500_000);
    assert_eq!(
        env.premium("daily").await.unwrap().calls,
        0,
        "tier from the persisted rows"
    );
}

#[tokio::test]
async fn a_snapshot_of_another_version_than_the_applied_one_is_not_used() {
    let (env, turn) = standard_world().await;
    let other_version = settlement_snapshot(
        8,
        vec![
            model("s", ModelTier::Standard)
                .mult(5_000_000_000, 5_000_000_000)
                .build(),
        ],
    );

    let won = finalize(
        &env,
        &StreamReq::completed(turn)
            .usage(900, 300)
            .snapshot(other_version),
    )
    .await;

    assert_eq!(won.settlement_method, SettlementMethod::Estimated);
    assert_eq!(won.committed_credits_micro, 1_500_000);
    assert_eq!(won.fallback, Some(FallbackCause::SnapshotUnavailable));
}

#[tokio::test]
async fn settlement_uses_the_multipliers_of_the_applied_version_snapshot() {
    let (env, turn) = standard_world().await;
    let version_7 = settlement_snapshot(
        7,
        vec![
            model("s", ModelTier::Standard)
                .mult(2_000_000_000, 4_000_000_000)
                .build(),
        ],
    );

    let won = finalize(
        &env,
        &StreamReq::completed(turn)
            .usage(900, 300)
            .snapshot(version_7),
    )
    .await;

    // 900 at 2.0 per 1K + 300 at 4.0 per 1K
    assert_eq!(won.settlement_method, SettlementMethod::Actual);
    assert_eq!(won.committed_credits_micro, 3_000_000);
}

// ---- credit errors -------------------------------------------------------------------------------------

#[tokio::test]
async fn a_token_count_above_ten_million_settles_estimated_without_rolling_back_and_names_the_value()
 {
    let capture = LogCapture::start();
    let (env, turn) = standard_world().await;

    let won = finalize(&env, &StreamReq::completed(turn).usage(10_000_001, 0)).await;

    assert_eq!(won.terminal_state, TerminalState::Completed);
    assert_eq!(won.settlement_method, SettlementMethod::Estimated);
    assert_eq!(won.committed_credits_micro, 1_500_000);
    assert_eq!(
        won.fallback,
        Some(FallbackCause::Credit(CreditError::InvalidTokenCount {
            field: "input_tokens",
            value: 10_000_001
        }))
    );
    assert_eq!(
        env.turn_state(turn).await,
        "completed",
        "the turn is not left running"
    );
    let (event, raw) = event_of(&env).await;
    assert_eq!(event.settlement_method, SettlementMethod::Estimated);
    assert_eq!(event.actual_credits_micro, 1_500_000);
    assert!(raw["usage"].is_null());
    assert!(
        !capture.at_containing(Level::ERROR, "10000001").is_empty(),
        "{}",
        capture.all_text()
    );
}

#[tokio::test]
async fn a_credit_error_takes_the_tier_from_the_snapshot_entry_not_from_the_rows() {
    // a premium turn whose premium rows do not exist
    let env = QuotaEnv::new().await;
    let chat = env.seed_chat(1).await;
    let turn = env.seed_turn(&TurnSeed::premium(1, chat)).await;
    env.seed_bucket(&BucketSeed::total("daily", 0, 3_750_000))
        .await;
    env.seed_bucket(&BucketSeed::total("monthly", 0, 3_750_000))
        .await;

    let won = finalize(&env, &StreamReq::completed(turn).usage(10_000_001, 0)).await;

    assert!(matches!(won.fallback, Some(FallbackCause::Credit(_))));
    for period in ["daily", "monthly"] {
        let premium = env.premium(period).await.expect(period);
        assert_eq!(
            (premium.spent, premium.reserved, premium.calls),
            (3_750_000, -3_750_000, 1),
            "{period}: the snapshot entry says premium"
        );
    }
}

#[tokio::test]
async fn an_unavailable_snapshot_without_premium_rows_is_a_standard_turn() {
    let env = QuotaEnv::new().await;
    let chat = env.seed_chat(1).await;
    let turn = env.seed_turn(&TurnSeed::premium(1, chat)).await;
    env.seed_bucket(&BucketSeed::total("daily", 0, 3_750_000))
        .await;
    env.seed_bucket(&BucketSeed::total("monthly", 0, 3_750_000))
        .await;

    finalize(
        &env,
        &StreamReq::completed(turn)
            .usage(900, 300)
            .snapshot(SettlementSnapshot::Unavailable),
    )
    .await;

    assert!(env.premium("daily").await.is_none());
    assert!(env.premium("monthly").await.is_none());
}

#[tokio::test]
async fn a_zero_or_overflowing_snapshot_multiplier_settles_estimated_at_the_reserve() {
    for multiplier in [0_u64, 10_000_001_000] {
        let (env, turn) = standard_world().await;
        let broken = settlement_snapshot(
            7,
            vec![model("s", ModelTier::Standard).mult(multiplier, 1).build()],
        );

        let won = finalize(
            &env,
            &StreamReq::completed(turn).usage(900, 300).snapshot(broken),
        )
        .await;

        assert_eq!(
            won.settlement_method,
            SettlementMethod::Estimated,
            "{multiplier}"
        );
        assert_eq!(won.committed_credits_micro, 1_500_000, "{multiplier}");
        assert!(
            matches!(won.fallback, Some(FallbackCause::Credit(_))),
            "{multiplier}"
        );
        assert_eq!(env.turn_state(turn).await, "completed", "{multiplier}");
        assert_eq!(env.usage_rows().await.len(), 1, "{multiplier}");
    }
}

#[tokio::test]
async fn the_fallback_never_evaluates_the_overshoot_cap() {
    let env = QuotaEnv::new().await;
    let chat = env.seed_chat(1).await;
    let turn = env
        .seed_turn(&TurnSeed::standard(1, chat).with_reserve(10_000, 500, 10_000_000))
        .await;
    env.seed_design_world(10_000_000, false).await;

    let won = finalize(
        &env,
        &StreamReq::completed(turn)
            .usage(11_000, 500)
            .snapshot(SettlementSnapshot::Unavailable),
    )
    .await;

    assert!(!won.overshoot_capped);
    assert_eq!(won.committed_credits_micro, 10_000_000);
}

// ---- released and the orphan variant ----------------------------------------------------------------------

#[tokio::test]
async fn a_released_settlement_never_falls_back_and_still_releases_the_premium_reserve() {
    let (env, turn) = premium_world().await;

    let won = finalize(
        &env,
        &StreamReq::failed(turn, "validation_error").snapshot(SettlementSnapshot::Unavailable),
    )
    .await;

    assert_eq!(won.settlement_method, SettlementMethod::Released);
    assert_eq!(won.committed_credits_micro, 0);
    assert_eq!(won.fallback, None);
    for (period, premium_spent) in [("daily", 20_000_000), ("monthly", 200_000_000)] {
        let premium = env.premium(period).await.unwrap();
        assert_eq!(
            (premium.spent, premium.reserved, premium.calls),
            (premium_spent, 0, 1),
            "{period}"
        );
    }
    let (event, _) = event_of(&env).await;
    assert_eq!(event.settlement_method, SettlementMethod::Released);
    assert_eq!(event.actual_credits_micro, 0);
}

#[tokio::test]
async fn a_stale_turn_is_finalized_failed_orphan_timeout_without_its_snapshot() {
    let env = QuotaEnv::new().await;
    let chat = env.seed_chat(1).await;
    let turn = env
        .seed_turn(
            &TurnSeed::standard(1, chat)
                .started("2026-03-10T11:00:00Z", Some("2026-03-10T11:10:00Z")),
        )
        .await;
    env.seed_design_world(1_500_000, false).await;
    let request = FinalizeRequest::orphan(
        turn,
        utc(2026, 3, 10, 11, 55, 0),
        SettlementSnapshot::Unavailable,
        noon(),
    );

    let won = won(env.repo.finalize_turn(&request).await.unwrap());

    assert_eq!(won.terminal_state, TerminalState::Failed);
    assert_eq!(won.billing_outcome, BillingOutcome::Aborted);
    assert_eq!(won.settlement_method, SettlementMethod::Estimated);
    assert_eq!(won.committed_credits_micro, 1_500_000);
    assert_eq!(env.turn_col(turn, "error_code").await, "'orphan_timeout'");
    assert_eq!(env.total("daily").await.unwrap().spent, 26_500_000);
}

#[tokio::test]
async fn the_same_row_always_commits_the_same_credits() {
    let mut outcomes = Vec::new();
    for _ in 0..2 {
        let (env, turn) = standard_world().await;
        let won = finalize(
            &env,
            &StreamReq::completed(turn)
                .usage(900, 300)
                .snapshot(SettlementSnapshot::Unavailable),
        )
        .await;
        let daily = env.total("daily").await.unwrap();
        let monthly = env.total("monthly").await.unwrap();
        outcomes.push((
            won.committed_credits_micro,
            (daily.spent, daily.reserved, daily.calls),
            (monthly.spent, monthly.reserved, monthly.calls),
        ));
    }

    assert_eq!(outcomes[0], outcomes[1]);
    assert_eq!(outcomes[0].0, 1_500_000);
}

// ---- through the service: the plugin is called before the transaction -----------------------------------------

#[tokio::test]
async fn a_failing_plugin_fetch_before_the_transaction_still_finalizes_the_stale_turn() {
    let capture = LogCapture::start();
    let env = QuotaEnv::new().await;
    let chat = env.seed_chat(1).await;
    let turn = env
        .seed_turn(
            &TurnSeed::standard(1, chat)
                .started("2026-03-10T11:00:00Z", Some("2026-03-10T11:10:00Z")),
        )
        .await;
    env.seed_design_world(1_500_000, false).await;
    env.policy.fail(PolicyStep::Snapshot);

    let snapshot = env
        .service
        .prefetch_settlement_snapshot(USER, turn, Some(7))
        .await;
    assert_eq!(snapshot, SettlementSnapshot::Unavailable);
    // the turn is still running while the plugin is called: no write has happened yet
    assert_eq!(env.turn_state(turn).await, "running");
    let request = FinalizeRequest::orphan(turn, utc(2026, 3, 10, 11, 55, 0), snapshot, noon());
    let result = env.service.finalize_turn(&request).await.unwrap();

    let FinalizeResult::Won(won) = result else {
        panic!("the CAS is won");
    };
    assert_eq!(won.settlement_method, SettlementMethod::Estimated);
    assert_eq!(won.committed_credits_micro, 1_500_000);
    assert_eq!(env.turn_state(turn).await, "failed");
    assert!(
        !capture
            .at_containing(Level::ERROR, &turn.to_string())
            .is_empty(),
        "{}",
        capture.all_text()
    );
    assert!(
        env.metrics.events().is_empty(),
        "an estimated settlement records no actual metric"
    );
}

#[tokio::test]
async fn the_orphan_settlement_uses_the_snapshot_of_the_applied_version_after_the_current_one_changed()
 {
    let env = QuotaEnv::new().await;
    let chat = env.seed_chat(1).await;
    let turn = env
        .seed_turn(
            &TurnSeed::standard(1, chat)
                .started("2026-03-10T11:00:00Z", Some("2026-03-10T11:10:00Z")),
        )
        .await;
    env.seed_design_world(1_500_000, false).await;
    // the current version is now 9 and prices the standard model five times higher
    env.policy.add_snapshot(snapshot(
        9,
        vec![
            model("s", ModelTier::Standard)
                .mult(5_000_000_000, 5_000_000_000)
                .build(),
        ],
    ));
    env.policy.set_current(9);

    let prefetched = env
        .service
        .prefetch_settlement_snapshot(USER, turn, Some(7))
        .await;
    let result = env
        .service
        .finalize_turn(&FinalizeRequest::orphan(
            turn,
            utc(2026, 3, 10, 11, 55, 0),
            prefetched,
            noon(),
        ))
        .await
        .unwrap();

    let FinalizeResult::Won(won) = result else {
        panic!("the CAS is won");
    };
    assert_eq!(
        won.committed_credits_micro, 1_050_000,
        "version 7 prices, not version 9"
    );
    assert_eq!(env.policy.calls(), vec![PolicyCall::Snapshot(USER, 7)]);
}

#[tokio::test]
async fn the_stream_finalization_reuses_the_preflight_snapshot_without_calling_the_plugin() {
    let (env, turn) = standard_world().await;

    let result = env
        .service
        .finalize_turn(&StreamReq::completed(turn).usage(900, 300).build())
        .await
        .unwrap();

    let FinalizeResult::Won(won) = result else {
        panic!("the CAS is won");
    };
    assert_eq!(won.committed_credits_micro, 1_200_000);
    assert!(env.policy.calls().is_empty(), "{:?}", env.policy.calls());
}

#[tokio::test]
async fn a_service_finalization_records_the_commit_metrics_only_for_an_actual_settlement() {
    let (env, turn) = standard_world().await;

    env.service
        .finalize_turn(&StreamReq::completed(turn).usage(900, 300).build())
        .await
        .unwrap();

    let events = env.metrics.events();
    assert!(
        events.contains(
            &crate::domain::service::quota::test_support::Metric::Commit(
                crate::domain::service::quota::types::QuotaPeriod::Daily
            )
        ),
        "{events:?}"
    );
    assert!(events.contains(&crate::domain::service::quota::test_support::Metric::Actual(1200)));
}
