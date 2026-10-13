#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Slice 4: the CAS-guarded finalization of `SeaQuotaRepository` on `SQLite`
//! (`cpt-cf-mini-chat-algo-quota-billing-finalize-turn`,
//! `cpt-cf-mini-chat-algo-quota-billing-settle`,
//! `cpt-cf-mini-chat-algo-quota-billing-turn-usage-event`,
//! `cpt-cf-mini-chat-dod-quota-billing-settlement`,
//! `cpt-cf-mini-chat-dod-quota-billing-usage-events`): the bucket deltas of
//! every settlement path, the usage event enqueued in the same transaction,
//! exactly-once finalization, the caller's steps in the same transaction, the
//! orphan variant and the corrupt-data branches. The deterministic fallback is
//! in `quota_fallback_tests`.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use futures::future::join_all;
use mini_chat_sdk::{
    BillingOutcome, RequesterType, SettlementMethod, TerminalState, UsageEvent, decode_usage_event,
    usage_dedupe_key,
};
use sea_orm::ActiveValue::Set;
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};
use serde_json::{Value, json};
use time::OffsetDateTime;
use toolkit_db::DbTx;
use toolkit_db::secure::{SecureEntityExt, secure_insert};
use toolkit_security::AccessScope;
use tracing::Level;
use uuid::Uuid;

use crate::chat_test_env::{OutboxRow, request_id};
use crate::config::QuotaConfig;
use crate::domain::error::DomainError;
use crate::domain::service::quota::finalize::{FinalizeResult, FinalizeWon};
use crate::domain::service::quota::ports::QuotaRepository;
use crate::domain::service::quota::service::QuotaServiceConfig;
use crate::domain::service::quota::test_support::{default_config, noon};
use crate::infra::db::entities::{chat_turn, message};
use crate::infra::db::repos::quota::TxFuture;
use crate::infra::outbox::queues::partition_for;
use crate::quota_test_env::{
    BucketSeed, NOON_TEXT, Options, QuotaEnv, StreamReq, TurnSeed, orphan_request,
};
use crate::test_doubles::{LogCapture, OTHER_TENANT, USER, USER_TENANT};

// ---- worlds -----------------------------------------------------------------------------------------

/// A standard turn of the DESIGN 5.10 numbers (reserved `1_500_000`) with its
/// reserve booked in the `total` rows; the premium rows hold `20_000_000` /
/// `200_000_000` spent and nothing reserved.
async fn standard_world() -> (QuotaEnv, Uuid) {
    standard_world_in(QuotaEnv::new().await).await
}

async fn standard_world_in(env: QuotaEnv) -> (QuotaEnv, Uuid) {
    let chat = env.seed_chat(1).await;
    let turn = env.seed_turn(&TurnSeed::standard(1, chat)).await;
    env.seed_design_world(1_500_000, false).await;
    (env, turn)
}

/// A premium turn (reserved `3_750_000`) booked in `total` and `tier:premium`.
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

async fn finalize(env: &QuotaEnv, request: &StreamReq) -> FinalizeResult {
    env.repo.finalize_turn(&request.build()).await.unwrap()
}

fn payload(row: &OutboxRow) -> Value {
    row.json()
}

fn keys(value: &Value) -> BTreeSet<String> {
    value.as_object().unwrap().keys().cloned().collect()
}

async fn only_event(env: &QuotaEnv) -> (UsageEvent, Value, OutboxRow) {
    let mut rows = env.usage_rows().await;
    assert_eq!(rows.len(), 1, "exactly one usage message");
    let row = rows.remove(0);
    let event = decode_usage_event(&row.payload).unwrap();
    let raw = payload(&row);
    (event, raw, row)
}

// ================================================================================================
// Settlement paths: bucket rows and events
// ================================================================================================

#[tokio::test]
async fn design_5_10_actual_settlement_moves_the_total_rows_and_leaves_the_premium_rows_alone() {
    let (env, turn) = standard_world().await;

    let result = finalize(&env, &StreamReq::completed(turn).usage(900, 300)).await;

    let won = won(result);
    assert_eq!(won.terminal_state, TerminalState::Completed);
    assert_eq!(won.billing_outcome, BillingOutcome::Completed);
    assert_eq!(won.settlement_method, SettlementMethod::Actual);
    assert_eq!(won.committed_credits_micro, 1_200_000);
    assert!(won.settled && won.usage_event_enqueued);
    assert_eq!(
        (won.actual_input_tokens, won.actual_output_tokens),
        (900, 300)
    );
    assert_eq!(won.reserve_tokens, 1500);
    assert!(!won.overshoot_capped);
    assert_eq!(won.fallback, None);
    assert_eq!(won.effective_model.as_deref(), Some("s"));

    let daily = env.total("daily").await.unwrap();
    assert_eq!(daily.spent, 26_200_000);
    assert_eq!(daily.reserved, 0, "back to the value before the reserve");
    assert_eq!(
        (daily.calls, daily.input_tokens, daily.output_tokens),
        (1, 900, 300)
    );
    let monthly = env.total("monthly").await.unwrap();
    assert_eq!(monthly.spent, 241_200_000);
    assert_eq!(monthly.reserved, 0);
    assert_eq!(
        (monthly.calls, monthly.input_tokens, monthly.output_tokens),
        (1, 900, 300)
    );
    for period in ["daily", "monthly"] {
        let premium = env.premium(period).await.unwrap();
        assert_eq!(premium.reserved, 0, "{period}");
        assert_eq!(premium.calls, 0, "{period}");
        assert_eq!(
            premium.spent,
            if period == "daily" {
                20_000_000
            } else {
                200_000_000
            }
        );
    }
}

#[tokio::test]
async fn a_premium_turn_also_moves_the_premium_rows_but_not_their_token_counters() {
    let (env, turn) = premium_world().await;

    let result = finalize(&env, &StreamReq::completed(turn).usage(900, 300)).await;

    // 1200 tokens at 2.5 credits per 1K
    assert_eq!(won(result).committed_credits_micro, 3_000_000);
    for (period, total_spent, premium_spent) in [
        ("daily", 28_000_000, 23_000_000),
        ("monthly", 243_000_000, 203_000_000),
    ] {
        let total = env.total(period).await.unwrap();
        assert_eq!(
            (
                total.spent,
                total.reserved,
                total.calls,
                total.input_tokens,
                total.output_tokens
            ),
            (total_spent, 0, 1, 900, 300),
            "{period} total"
        );
        let premium = env.premium(period).await.unwrap();
        assert_eq!(
            (premium.spent, premium.reserved, premium.calls),
            (premium_spent, 0, 1),
            "{period} premium"
        );
        assert_eq!(
            (
                premium.input_tokens,
                premium.output_tokens,
                premium.web,
                premium.ci
            ),
            (0, 0, 0, 0),
            "{period}: the premium row keeps no token or tool counters"
        );
    }
}

#[tokio::test]
async fn the_usage_event_is_enqueued_with_exactly_the_documented_fields() {
    let (env, turn) = standard_world().await;

    finalize(
        &env,
        &StreamReq::completed(turn).usage(900, 300).tools(0, 0, 0),
    )
    .await;

    let (event, raw, row) = only_event(&env).await;
    let expected_keys: BTreeSet<String> = [
        "tenant_id",
        "user_id",
        "chat_id",
        "turn_id",
        "request_id",
        "effective_model",
        "selected_model",
        "terminal_state",
        "billing_outcome",
        "usage",
        "actual_credits_micro",
        "settlement_method",
        "policy_version_applied",
        "web_search_calls",
        "code_interpreter_calls",
        "file_search_calls",
        "timestamp",
        "requester_type",
        "dedupe_key",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect();
    assert_eq!(keys(&raw), expected_keys);
    assert_eq!(event.tenant_id, USER_TENANT);
    assert_eq!(event.user_id, Some(USER));
    assert_eq!(event.chat_id, crate::chat_test_env::chat_id(1));
    assert_eq!(event.turn_id, Some(turn));
    assert_eq!(event.request_id, request_id(1));
    assert_eq!(event.effective_model, "s");
    assert_eq!(event.selected_model, "p");
    assert_eq!(event.terminal_state, TerminalState::Completed);
    assert_eq!(event.billing_outcome, BillingOutcome::Completed);
    assert_eq!(event.settlement_method, SettlementMethod::Actual);
    assert_eq!(event.actual_credits_micro, 1_200_000);
    assert_eq!(event.policy_version_applied, 7);
    assert_eq!(event.usage.unwrap().input_tokens, 900);
    assert_eq!(event.usage.unwrap().output_tokens, 300);
    assert_eq!(event.requester_type, RequesterType::User);
    assert_eq!(event.system_task_type, None);
    assert_eq!(event.timestamp, noon());
    assert_eq!(
        event.dedupe_key,
        usage_dedupe_key(USER_TENANT, turn, request_id(1))
    );
    assert_eq!(raw["timestamp"], NOON_TEXT);
    assert_eq!(row.queue, "mini-chat.usage_snapshot");
    assert_eq!(row.partition, i64::from(partition_for(USER_TENANT, 4)));
}

#[tokio::test]
async fn the_event_never_carries_reserve_or_decision_fields() {
    let (env, turn) = standard_world().await;

    finalize(&env, &StreamReq::failed(turn, "provider_error")).await;

    let (_, raw, _) = only_event(&env).await;
    for absent in [
        "event_type",
        "reserved_credits_micro",
        "reserve_tokens",
        "committed_credits_micro",
        "overshoot_capped",
        "error_code",
        "completion_signal",
        "quota_decision",
        "downgrade_from",
        "downgrade_reason",
    ] {
        assert!(raw.get(absent).is_none(), "{absent}");
    }
}

#[tokio::test]
async fn a_completed_turn_without_usage_settles_actual_at_zero_credits() {
    let (env, turn) = standard_world().await;

    let result = finalize(&env, &StreamReq::completed(turn)).await;

    let won = won(result);
    assert_eq!(won.settlement_method, SettlementMethod::Actual);
    assert_eq!(won.committed_credits_micro, 0);
    let daily = env.total("daily").await.unwrap();
    assert_eq!(
        (daily.spent, daily.reserved, daily.calls),
        (25_000_000, 0, 1)
    );
    assert_eq!((daily.input_tokens, daily.output_tokens), (0, 0));
    let (event, raw, _) = only_event(&env).await;
    assert_eq!(event.settlement_method, SettlementMethod::Actual);
    assert_eq!(event.actual_credits_micro, 0);
    assert!(raw["usage"].is_null());
}

#[tokio::test]
async fn the_cas_writes_the_terminal_columns_and_the_completion_time() {
    let (env, turn) = standard_world().await;

    finalize(&env, &StreamReq::completed(turn).usage(900, 300)).await;

    assert_eq!(env.turn_state(turn).await, "completed");
    assert_eq!(env.turn_col(turn, "error_code").await, "NULL");
    assert_eq!(env.turn_col(turn, "error_detail").await, "NULL");
    assert_eq!(env.turn_col(turn, "provider_response_id").await, "'resp_1'");
    assert_eq!(
        env.turn_col(turn, "assistant_message_id").await,
        format!("X'{}'", Uuid::from_u128(0xB000_0001).simple()).to_uppercase()
    );
    assert_eq!(
        env.turn_col(turn, "completed_at").await,
        format!("'{NOON_TEXT}'")
    );
    assert_eq!(
        env.turn_col(turn, "updated_at").await,
        format!("'{NOON_TEXT}'")
    );
    // the persisted preflight columns are never changed
    assert_eq!(env.turn_col(turn, "reserve_tokens").await, "1500");
    assert_eq!(
        env.turn_col(turn, "reserved_credits_micro").await,
        "1500000"
    );
    assert_eq!(env.turn_col(turn, "effective_model").await, "'s'");
    assert_eq!(env.turn_col(turn, "policy_version_applied").await, "7");
}

// ---- failures ----------------------------------------------------------------------------------------

#[tokio::test]
async fn a_post_provider_failure_with_usage_settles_actual() {
    let (env, turn) = standard_world().await;

    let result = finalize(
        &env,
        &StreamReq::failed(turn, "provider_error").usage(500, 100),
    )
    .await;

    let won = won(result);
    assert_eq!(won.settlement_method, SettlementMethod::Actual);
    assert_eq!(won.committed_credits_micro, 600_000);
    assert_eq!(env.turn_state(turn).await, "failed");
    assert_eq!(env.turn_col(turn, "error_code").await, "'provider_error'");
    assert_eq!(env.turn_col(turn, "error_detail").await, "'detail'");
    let (event, _, _) = only_event(&env).await;
    assert_eq!(event.terminal_state, TerminalState::Failed);
    assert_eq!(event.billing_outcome, BillingOutcome::Failed);
    assert_eq!(event.settlement_method, SettlementMethod::Actual);
    assert_eq!(event.usage.unwrap().input_tokens, 500);
}

#[tokio::test]
async fn a_post_provider_failure_without_usage_or_with_zero_usage_settles_estimated() {
    for usage in [None, Some((0_u64, 0_u64))] {
        let (env, turn) = standard_world().await;
        let mut request = StreamReq::failed(turn, "provider_timeout");
        if let Some((input, output)) = usage {
            request = request.usage(input, output);
        }

        let result = finalize(&env, &request).await;

        let won = won(result);
        assert_eq!(
            won.settlement_method,
            SettlementMethod::Estimated,
            "{usage:?}"
        );
        // estimated input 1000 + floor 50 at 1.0 credit per 1K tokens
        assert_eq!(won.committed_credits_micro, 1_050_000, "{usage:?}");
        let daily = env.total("daily").await.unwrap();
        assert_eq!(daily.spent, 26_050_000, "{usage:?}");
        assert_eq!(daily.reserved, 0);
        assert_eq!(
            (daily.input_tokens, daily.output_tokens),
            (0, 0),
            "no token telemetry"
        );
        let (event, raw, _) = only_event(&env).await;
        assert_eq!(event.settlement_method, SettlementMethod::Estimated);
        assert_eq!(event.actual_credits_micro, 1_050_000);
        assert!(raw["usage"].is_null());
    }
}

#[tokio::test]
async fn a_client_disconnect_is_aborted_and_estimated_ignoring_the_usage_passed() {
    let (env, turn) = standard_world().await;

    let result = finalize(&env, &StreamReq::cancelled(turn).usage(900, 300)).await;

    let won = won(result);
    assert_eq!(won.terminal_state, TerminalState::Cancelled);
    assert_eq!(won.billing_outcome, BillingOutcome::Aborted);
    assert_eq!(won.settlement_method, SettlementMethod::Estimated);
    assert_eq!(won.committed_credits_micro, 1_050_000);
    assert_eq!(env.turn_state(turn).await, "cancelled");
    let (event, raw, _) = only_event(&env).await;
    assert_eq!(event.terminal_state, TerminalState::Cancelled);
    assert_eq!(event.billing_outcome, BillingOutcome::Aborted);
    assert!(raw["usage"].is_null());
    assert_eq!(env.total("daily").await.unwrap().input_tokens, 0);
}

#[tokio::test]
async fn a_pre_provider_failure_releases_the_reserve_and_adds_no_tokens_or_tool_calls() {
    let (env, turn) = standard_world().await;

    let result = finalize(
        &env,
        &StreamReq::failed(turn, "context_length_exceeded").tools(2, 1, 1),
    )
    .await;

    let won = won(result);
    assert_eq!(won.settlement_method, SettlementMethod::Released);
    assert_eq!(won.committed_credits_micro, 0);
    for period in ["daily", "monthly"] {
        let total = env.total(period).await.unwrap();
        assert_eq!(total.reserved, 0, "{period}: the reserve is released");
        assert_eq!(total.calls, 1, "{period}: calls counts every settlement");
        assert_eq!(
            (total.input_tokens, total.output_tokens, total.web, total.ci),
            (0, 0, 0, 0),
            "{period}"
        );
    }
    assert_eq!(env.total("daily").await.unwrap().spent, 25_000_000);
    let (event, _, _) = only_event(&env).await;
    assert_eq!(event.settlement_method, SettlementMethod::Released);
    assert_eq!(event.actual_credits_micro, 0);
    let usage = event.usage.expect("released reports zero usage");
    assert_eq!(
        (
            usage.input_tokens,
            usage.output_tokens,
            usage.cache_read_input_tokens
        ),
        (0, 0, 0)
    );
}

#[tokio::test]
async fn an_unknown_error_code_settles_failed_estimated_and_is_logged_at_error() {
    let capture = LogCapture::start();
    let (env, turn) = standard_world().await;

    let result = finalize(&env, &StreamReq::failed(turn, "quota_exceeded")).await;

    let won = won(result);
    assert_eq!(won.billing_outcome, BillingOutcome::Failed);
    assert_eq!(won.settlement_method, SettlementMethod::Estimated);
    assert!(
        !capture
            .at_containing(Level::ERROR, "quota_exceeded")
            .is_empty(),
        "{}",
        capture.all_text()
    );
}

// ---- tool calls --------------------------------------------------------------------------------------

#[tokio::test]
async fn completed_tool_calls_count_into_the_total_rows_of_both_periods_and_the_event() {
    let (env, turn) = standard_world().await;

    finalize(
        &env,
        &StreamReq::completed(turn).usage(900, 300).tools(2, 1, 1),
    )
    .await;

    for period in ["daily", "monthly"] {
        let total = env.total(period).await.unwrap();
        assert_eq!((total.web, total.ci), (2, 1), "{period}");
        assert_eq!(
            total.file_search_calls, 0,
            "{period}: file search is never counted"
        );
        assert_eq!(
            (
                total.rag_retrieval_calls,
                total.image_inputs,
                total.image_upload_bytes
            ),
            (0, 0, 0)
        );
        let premium = env.premium(period).await.unwrap();
        assert_eq!((premium.web, premium.ci), (0, 0), "{period}");
    }
    let (event, _, _) = only_event(&env).await;
    assert_eq!(
        (
            event.web_search_calls,
            event.code_interpreter_calls,
            event.file_search_calls
        ),
        (2, 1, 1)
    );
}

#[tokio::test]
async fn tool_calls_also_count_on_an_estimated_settlement() {
    let (env, turn) = standard_world().await;

    finalize(&env, &StreamReq::cancelled(turn).tools(1, 3, 0)).await;

    let daily = env.total("daily").await.unwrap();
    assert_eq!((daily.web, daily.ci), (1, 3));
}

#[tokio::test]
async fn tool_counters_accumulate_on_top_of_the_existing_row_values() {
    let env = QuotaEnv::new().await;
    let chat = env.seed_chat(1).await;
    let turn = env.seed_turn(&TurnSeed::standard(1, chat)).await;
    env.seed_bucket(
        &BucketSeed::total("daily", 0, 1_500_000)
            .counters(4, 10, 20)
            .tools(70, 40),
    )
    .await;
    env.seed_bucket(&BucketSeed::total("monthly", 0, 1_500_000))
        .await;

    finalize(
        &env,
        &StreamReq::completed(turn).usage(900, 300).tools(2, 1, 0),
    )
    .await;

    let daily = env.total("daily").await.unwrap();
    assert_eq!((daily.web, daily.ci), (72, 41));
    assert_eq!(
        (daily.calls, daily.input_tokens, daily.output_tokens),
        (5, 910, 320)
    );
}

// ---- the overshoot cap --------------------------------------------------------------------------------

async fn overshoot_world(options: Options) -> (QuotaEnv, Uuid) {
    let env = QuotaEnv::with(options).await;
    let chat = env.seed_chat(1).await;
    let turn = env
        .seed_turn(&TurnSeed::standard(1, chat).with_reserve(10_000, 500, 10_000_000))
        .await;
    env.seed_design_world(10_000_000, false).await;
    (env, turn)
}

#[tokio::test]
async fn an_overshoot_beyond_the_tolerance_commits_the_reserve_keeps_the_turn_completed_and_counts_the_tokens()
 {
    let (env, turn) = overshoot_world(Options::default()).await;

    // 11_500 / 10_000 = 1.15 > 1.10
    let result = finalize(&env, &StreamReq::completed(turn).usage(11_000, 500)).await;

    let won = won(result);
    assert!(won.overshoot_capped);
    assert_eq!(won.committed_credits_micro, 10_000_000);
    assert_eq!(won.terminal_state, TerminalState::Completed);
    assert_eq!(env.turn_state(turn).await, "completed");
    let daily = env.total("daily").await.unwrap();
    assert_eq!(daily.spent, 35_000_000, "25_000_000 + the reserve");
    assert_eq!((daily.input_tokens, daily.output_tokens), (11_000, 500));
    let (event, _, _) = only_event(&env).await;
    assert_eq!(event.actual_credits_micro, 10_000_000);
    assert_eq!(event.usage.unwrap().input_tokens, 11_000);
}

#[tokio::test]
async fn an_overshoot_of_exactly_the_tolerance_commits_the_actual_credits() {
    let (env, turn) = overshoot_world(Options::default()).await;

    // 11_000 / 10_000 = 1.10 exactly
    let result = finalize(&env, &StreamReq::completed(turn).usage(10_500, 500)).await;

    let won = won(result);
    assert!(!won.overshoot_capped);
    assert_eq!(won.committed_credits_micro, 11_000_000);
    let daily = env.total("daily").await.unwrap();
    assert_eq!(
        daily.spent, 36_000_000,
        "the actual credits exceed the reserve"
    );
    assert_eq!(daily.reserved, 0);
}

#[tokio::test]
async fn the_tolerance_factor_comes_from_the_configuration() {
    let options = Options {
        config: QuotaServiceConfig {
            quota: QuotaConfig {
                overshoot_tolerance_factor: 1.5,
                ..QuotaConfig::default()
            },
            ..default_config()
        },
        ..Options::default()
    };
    let (env, turn) = overshoot_world(options).await;

    let result = finalize(&env, &StreamReq::completed(turn).usage(11_000, 500)).await;

    let won = won(result);
    assert!(!won.overshoot_capped, "1.15 is within 1.5");
    assert_eq!(won.committed_credits_micro, 11_500_000);
}

// ---- the cost record ----------------------------------------------------------------------------------

#[tokio::test]
async fn every_settlement_writes_one_quota_settled_record_with_the_request_values() {
    let capture = LogCapture::start();
    let (env, turn) = standard_world().await;

    finalize(
        &env,
        &StreamReq::completed(turn).usage(900, 300).tools(2, 1, 1),
    )
    .await;

    let records = capture.at_containing(Level::INFO, "quota settled");
    assert_eq!(records.len(), 1, "{}", capture.all_text());
    let text = records[0].text().to_lowercase();
    for expected in [
        turn.to_string(),
        crate::chat_test_env::chat_id(1).to_string(),
        request_id(1).to_string(),
        "reserved_credits_micro=1500000".to_owned(),
        "committed_credits_micro=1200000".to_owned(),
        "released_credits_micro=300000".to_owned(),
        "input_tokens=900".to_owned(),
        "output_tokens=300".to_owned(),
        "web_search_calls=2".to_owned(),
        "code_interpreter_calls=1".to_owned(),
        "file_search_calls=1".to_owned(),
        "settlement_method=actual".to_owned(),
        "billing_outcome=completed".to_owned(),
    ] {
        assert!(text.contains(&expected), "{expected} in {text}");
    }
    assert!(
        !text.contains(&USER.to_string()),
        "no user id in the record"
    );
    assert!(
        !text.contains(&USER_TENANT.to_string()),
        "no tenant id in the record"
    );
}

#[tokio::test]
async fn the_released_credits_never_go_below_zero_when_the_actual_credits_exceed_the_reserve() {
    let capture = LogCapture::start();
    let (env, turn) = overshoot_world(Options::default()).await;

    finalize(&env, &StreamReq::completed(turn).usage(10_500, 500)).await;

    let text = capture
        .at_containing(Level::INFO, "quota settled")
        .remove(0)
        .text();
    assert!(text.contains("released_credits_micro=0"), "{text}");
    assert!(text.contains("committed_credits_micro=11000000"), "{text}");
}

// ================================================================================================
// Exactly once
// ================================================================================================

#[tokio::test]
async fn a_second_finalizer_loses_and_changes_nothing() {
    let (env, turn) = standard_world().await;
    finalize(&env, &StreamReq::completed(turn).usage(900, 300)).await;
    let quota = env.quota_dump().await;
    let turns = env.dump("chat_turns").await;
    let capture = LogCapture::start();

    let second = finalize(&env, &StreamReq::cancelled(turn)).await;

    assert_eq!(second, FinalizeResult::Lost);
    assert_eq!(env.quota_dump().await, quota);
    assert_eq!(env.dump("chat_turns").await, turns);
    assert_eq!(env.usage_rows().await.len(), 1, "no second usage event");
    assert!(
        capture
            .at_containing(Level::INFO, "quota settled")
            .is_empty(),
        "no cost record for a lost CAS"
    );
}

#[tokio::test]
async fn a_turn_that_is_not_running_or_missing_is_lost_without_any_write() {
    let env = QuotaEnv::new().await;
    let chat = env.seed_chat(1).await;
    let terminal = env
        .seed_turn(&TurnSeed::standard(1, chat).state("completed"))
        .await;
    env.seed_design_world(1_500_000, false).await;
    let quota = env.quota_dump().await;

    for turn in [terminal, Uuid::from_u128(0xDEAD)] {
        let result = finalize(&env, &StreamReq::completed(turn).usage(1, 1)).await;

        assert_eq!(result, FinalizeResult::Lost, "{turn}");
    }
    assert_eq!(env.quota_dump().await, quota);
    assert!(env.usage_rows().await.is_empty());
}

#[tokio::test]
async fn a_stream_finalization_of_a_foreign_tenant_turn_is_lost_and_changes_nothing() {
    let (env, turn) = standard_world().await;
    let quota = env.quota_dump().await;
    let turns = env.dump("chat_turns").await;
    let outbox = env.outbox_message_count().await;

    let result = finalize(
        &env,
        &StreamReq::completed(turn)
            .tenant(OTHER_TENANT)
            .usage(900, 300),
    )
    .await;

    assert_eq!(result, FinalizeResult::Lost);
    assert_eq!(env.turn_state(turn).await, "running");
    assert_eq!(env.dump("chat_turns").await, turns);
    assert_eq!(env.quota_dump().await, quota);
    assert_eq!(env.outbox_message_count().await, outbox);
    assert!(env.usage_rows().await.is_empty());
    // control: the owning tenant finalizes the same turn
    assert!(matches!(
        finalize(&env, &StreamReq::completed(turn).usage(900, 300)).await,
        FinalizeResult::Won(_)
    ));
}

#[tokio::test]
async fn two_finalizers_racing_on_one_turn_produce_one_settlement_and_one_event() {
    let env = QuotaEnv::file().await;
    let (env, turn) = standard_world_in(env).await;
    let completed = StreamReq::completed(turn).usage(900, 300).build();
    let cancelled = StreamReq::cancelled(turn).build();

    let (first, second) = tokio::join!(
        env.repo.finalize_turn(&completed),
        env.repo.finalize_turn(&cancelled)
    );

    let (first, second) = (first.unwrap(), second.unwrap());
    let winners = [&first, &second]
        .into_iter()
        .filter(|r| matches!(r, FinalizeResult::Won(_)))
        .count();
    assert_eq!(winners, 1, "{first:?} / {second:?}");
    assert_eq!(env.usage_rows().await.len(), 1);
    let daily = env.total("daily").await.unwrap();
    assert_eq!(daily.reserved, 0);
    assert_eq!(daily.calls, 1, "settled once");
    let winner = if let FinalizeResult::Won(won) = &first {
        won
    } else {
        match &second {
            FinalizeResult::Won(won) => won,
            FinalizeResult::Lost => unreachable!(),
        }
    };
    assert_eq!(daily.spent, 25_000_000 + winner.committed_credits_micro);
}

#[tokio::test]
async fn many_turns_each_raced_by_two_finalizers_settle_exactly_once_each() {
    let env = QuotaEnv::file().await;
    let mut turns = Vec::new();
    for n in 1..=4_u128 {
        let chat = env.seed_chat(n).await;
        turns.push(env.seed_turn(&TurnSeed::standard(n, chat)).await);
    }
    env.seed_bucket(&BucketSeed::total("daily", 0, 6_000_000))
        .await;
    env.seed_bucket(&BucketSeed::total("monthly", 0, 6_000_000))
        .await;
    let requests: Vec<_> = turns
        .iter()
        .flat_map(|turn| {
            [
                StreamReq::completed(*turn).usage(900, 300).build(),
                StreamReq::cancelled(*turn).build(),
            ]
        })
        .collect();

    let results = join_all(requests.iter().map(|r| env.repo.finalize_turn(r))).await;

    let results: Vec<_> = results.into_iter().map(Result::unwrap).collect();
    let committed: i64 = results
        .iter()
        .filter_map(|r| match r {
            FinalizeResult::Won(won) => Some(won.committed_credits_micro),
            FinalizeResult::Lost => None,
        })
        .sum();
    assert_eq!(
        results
            .iter()
            .filter(|r| matches!(r, FinalizeResult::Won(_)))
            .count(),
        4
    );
    assert_eq!(env.usage_rows().await.len(), 4);
    let daily = env.total("daily").await.unwrap();
    assert_eq!(
        (daily.reserved, daily.calls, daily.spent),
        (0, 4, committed)
    );
    let monthly = env.total("monthly").await.unwrap();
    assert_eq!(
        (monthly.reserved, monthly.calls, monthly.spent),
        (0, 4, committed)
    );
}

// ================================================================================================
// The caller's steps in the same transaction
// ================================================================================================

fn insert_assistant<'a>(
    tx: &'a DbTx<'a>,
    id: Uuid,
    chat: Uuid,
    now: OffsetDateTime,
) -> TxFuture<'a> {
    Box::pin(async move {
        let row = message::ActiveModel {
            id: Set(id),
            tenant_id: Set(USER_TENANT),
            chat_id: Set(chat),
            request_id: Set(Some(request_id(1))),
            role: Set("assistant".to_owned()),
            content: Set("hello".to_owned()),
            content_type: Set("text".to_owned()),
            token_estimate: Set(0),
            provider_response_id: Set(None),
            request_kind: Set("chat".to_owned()),
            features_used: Set(json!({})),
            input_tokens: Set(900),
            output_tokens: Set(300),
            cache_read_input_tokens: Set(0),
            cache_write_input_tokens: Set(0),
            reasoning_tokens: Set(0),
            model: Set(Some("s".to_owned())),
            is_compressed: Set(false),
            created_at: Set(now),
            deleted_at: Set(None),
        };
        secure_insert::<message::Entity>(row, &AccessScope::for_tenant(USER_TENANT), tx)
            .await
            .map(|_| ())
            .map_err(|error| DomainError::Database(error.to_string()))
    })
}

#[tokio::test]
async fn the_caller_inserts_the_assistant_message_in_the_same_transaction() {
    let (env, turn) = standard_world().await;
    let chat = crate::chat_test_env::chat_id(1);
    let assistant = Uuid::from_u128(0xB000_0001);
    let seen_state = Arc::new(std::sync::Mutex::new(Vec::new()));
    let seen_in = Arc::clone(&seen_state);

    let result = env
        .repo
        .finalize_turn_with(
            &StreamReq::completed(turn).usage(900, 300).build(),
            move |tx, won| {
                let seen = Arc::clone(&seen_in);
                let committed = won.committed_credits_micro;
                Box::pin(async move {
                    let row = chat_turn::Entity::find()
                        .filter(chat_turn::Column::Id.eq(turn))
                        .secure()
                        .scope_with(&AccessScope::for_tenant(USER_TENANT))
                        .one(tx)
                        .await
                        .map_err(|e| DomainError::Database(e.to_string()))?
                        .unwrap();
                    seen.lock().unwrap().push((row.state, committed));
                    insert_assistant(tx, assistant, chat, noon()).await
                })
            },
        )
        .await
        .unwrap();

    assert!(matches!(result, FinalizeResult::Won(_)));
    assert_eq!(
        *seen_state.lock().unwrap(),
        vec![("completed".to_owned(), 1_200_000)],
        "the step sees the CAS and the settlement result of its own transaction"
    );
    assert_eq!(env.db.count("messages").await, 1);
    assert_eq!(env.usage_rows().await.len(), 1);
    assert_eq!(env.total("daily").await.unwrap().spent, 26_200_000);
}

#[tokio::test]
async fn a_failing_caller_step_rolls_back_the_cas_the_settlement_and_the_usage_event() {
    let (env, turn) = standard_world().await;
    let chat = crate::chat_test_env::chat_id(1);
    let quota = env.quota_dump().await;
    let turns = env.dump("chat_turns").await;

    let result = env
        .repo
        .finalize_turn_with(
            &StreamReq::completed(turn).usage(900, 300).build(),
            move |tx, _won| {
                Box::pin(async move {
                    insert_assistant(tx, Uuid::from_u128(0xB000_0001), chat, noon()).await?;
                    Err(DomainError::Internal("the audit enqueue failed".to_owned()))
                })
            },
        )
        .await;

    assert!(
        matches!(&result, Err(DomainError::Internal(text)) if text.contains("audit enqueue")),
        "{result:?}"
    );
    assert_eq!(
        env.turn_state(turn).await,
        "running",
        "the CAS is rolled back"
    );
    assert_eq!(env.quota_dump().await, quota);
    assert_eq!(env.dump("chat_turns").await, turns);
    assert_eq!(env.db.count("messages").await, 0);
    assert!(
        env.usage_rows().await.is_empty(),
        "no usage message without the settlement"
    );

    // the turn is not stuck: the next finalizer wins
    let retry = finalize(&env, &StreamReq::completed(turn).usage(900, 300)).await;
    assert!(matches!(retry, FinalizeResult::Won(_)));
    assert_eq!(env.usage_rows().await.len(), 1);
}

#[tokio::test]
async fn the_caller_step_is_not_called_when_the_cas_is_lost() {
    let (env, turn) = standard_world().await;
    finalize(&env, &StreamReq::completed(turn).usage(900, 300)).await;
    let calls = Arc::new(AtomicU32::new(0));
    let calls_in = Arc::clone(&calls);

    let result = env
        .repo
        .finalize_turn_with(&StreamReq::cancelled(turn).build(), move |_tx, _won| {
            calls_in.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { Ok(()) })
        })
        .await
        .unwrap();

    assert_eq!(result, FinalizeResult::Lost);
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn a_conflict_re_runs_the_whole_finalization_and_settles_exactly_once() {
    let (env, turn) = standard_world().await;
    let attempts = Arc::new(AtomicU32::new(0));
    let attempts_in = Arc::clone(&attempts);

    let result = env
        .repo
        .finalize_turn_with(
            &StreamReq::completed(turn).usage(900, 300).build(),
            move |_tx, _won| {
                let attempt = attempts_in.fetch_add(1, Ordering::SeqCst) + 1;
                Box::pin(async move {
                    if attempt <= 2 {
                        Err(DomainError::Database(
                            "error returned from database: (code: 5) database is locked".to_owned(),
                        ))
                    } else {
                        Ok(())
                    }
                })
            },
        )
        .await
        .unwrap();

    assert!(matches!(result, FinalizeResult::Won(_)));
    assert_eq!(attempts.load(Ordering::SeqCst), 3);
    let daily = env.total("daily").await.unwrap();
    assert_eq!(
        (daily.spent, daily.calls, daily.reserved),
        (26_200_000, 1, 0)
    );
    assert_eq!(env.usage_rows().await.len(), 1);
}

#[tokio::test]
async fn a_conflict_that_persists_leaves_the_turn_running_for_the_watchdog() {
    let (env, turn) = standard_world().await;
    let attempts = Arc::new(AtomicU32::new(0));
    let attempts_in = Arc::clone(&attempts);
    let quota = env.quota_dump().await;

    let result = env
        .repo
        .finalize_turn_with(&StreamReq::completed(turn).build(), move |_tx, _won| {
            attempts_in.fetch_add(1, Ordering::SeqCst);
            Box::pin(async {
                Err(DomainError::Database(
                    "error returned from database: (code: 5) database is locked".to_owned(),
                ))
            })
        })
        .await;

    assert!(
        matches!(result, Err(DomainError::Database(_))),
        "{result:?}"
    );
    assert_eq!(attempts.load(Ordering::SeqCst), 3);
    assert_eq!(env.turn_state(turn).await, "running");
    assert_eq!(env.quota_dump().await, quota);
    assert!(env.usage_rows().await.is_empty());
}

#[tokio::test]
async fn a_contention_raised_by_the_usage_enqueue_re_runs_the_finalization() {
    use sea_orm::ConnectionTrait;
    let (env, turn) = standard_world().await;
    env.db
        .raw
        .execute_unprepared(
            "CREATE TRIGGER outbox_busy BEFORE INSERT ON mini_chat__outbox_body \
             BEGIN SELECT RAISE(ABORT, '(code: 5) database is locked'); END",
        )
        .await
        .expect("create trigger");
    let quota = env.quota_dump().await;
    let capture = LogCapture::start();

    let result = env
        .repo
        .finalize_turn(&StreamReq::completed(turn).usage(900, 300).build())
        .await;

    assert!(
        matches!(result, Err(DomainError::Database(_))),
        "{result:?}"
    );
    assert_eq!(
        capture
            .at_containing(Level::WARN, "quota transaction conflict, retrying")
            .len(),
        2,
        "the enqueue contention is retried until the attempts are exhausted"
    );
    assert_eq!(env.turn_state(turn).await, "running");
    assert_eq!(env.quota_dump().await, quota);
    assert_eq!(env.outbox_message_count().await, 0);
}

// ================================================================================================
// The orphan variant
// ================================================================================================

/// A stale standard turn that started on the previous UTC day.
fn stale_seed(chat: Uuid) -> TurnSeed {
    TurnSeed::standard(1, chat).started("2026-03-09T23:59:50Z", None)
}

async fn orphan_world(seed: impl FnOnce(Uuid) -> TurnSeed) -> (QuotaEnv, Uuid) {
    let env = QuotaEnv::new().await;
    let chat = env.seed_chat(1).await;
    let turn = env.seed_turn(&seed(chat)).await;
    // the reserve was booked the day before: daily 2026-03-09, monthly 2026-03-01
    env.seed_bucket(&BucketSeed::total("daily", 25_000_000, 1_500_000).at("2026-03-09"))
        .await;
    env.seed_bucket(&BucketSeed::total("monthly", 240_000_000, 1_500_000))
        .await;
    (env, turn)
}

#[tokio::test]
async fn the_watchdog_finalizes_failed_orphan_timeout_aborted_estimated_on_the_preflight_periods() {
    let (env, turn) = orphan_world(stale_seed).await;

    let result = env.repo.finalize_turn(&orphan_request(turn)).await.unwrap();

    let won = won(result);
    assert_eq!(won.terminal_state, TerminalState::Failed);
    assert_eq!(won.billing_outcome, BillingOutcome::Aborted);
    assert_eq!(won.settlement_method, SettlementMethod::Estimated);
    assert_eq!(won.committed_credits_micro, 1_050_000);
    assert_eq!(env.turn_state(turn).await, "failed");
    assert_eq!(env.turn_col(turn, "error_code").await, "'orphan_timeout'");
    assert_eq!(
        env.turn_col(turn, "completed_at").await,
        format!("'{NOON_TEXT}'")
    );
    // the rows of the day the turn started on are settled ...
    let day_before = env
        .bucket_of(USER_TENANT, USER, "daily", "2026-03-09", "total")
        .await
        .unwrap();
    assert_eq!(
        (day_before.spent, day_before.reserved, day_before.calls),
        (26_050_000, 0, 1)
    );
    let monthly = env.total("monthly").await.unwrap();
    assert_eq!(
        (monthly.spent, monthly.reserved, monthly.calls),
        (241_050_000, 0, 1)
    );
    // ... and no row of the current day is created
    assert!(env.total("daily").await.is_none());
}

#[tokio::test]
async fn the_orphan_event_names_the_effective_model_as_the_selected_model_and_has_no_usage() {
    let (env, turn) = orphan_world(stale_seed).await;

    env.repo.finalize_turn(&orphan_request(turn)).await.unwrap();

    let (event, raw, _) = only_event(&env).await;
    assert_eq!(event.terminal_state, TerminalState::Failed);
    assert_eq!(event.billing_outcome, BillingOutcome::Aborted);
    assert_eq!(event.settlement_method, SettlementMethod::Estimated);
    assert_eq!(event.effective_model, "s");
    assert_eq!(event.selected_model, "s");
    assert_eq!(event.actual_credits_micro, 1_050_000);
    assert_eq!(event.policy_version_applied, 7);
    assert!(raw["usage"].is_null());
    assert_eq!(
        event.dedupe_key,
        usage_dedupe_key(USER_TENANT, turn, request_id(1))
    );
}

#[tokio::test]
async fn the_orphan_variant_takes_the_tool_counts_from_the_turn_row() {
    let (env, turn) = orphan_world(|chat| stale_seed(chat).tools_done(1, 2, 3)).await;

    env.repo.finalize_turn(&orphan_request(turn)).await.unwrap();

    let (event, _, _) = only_event(&env).await;
    assert_eq!(
        (
            event.web_search_calls,
            event.code_interpreter_calls,
            event.file_search_calls
        ),
        (1, 2, 3)
    );
    let monthly = env.total("monthly").await.unwrap();
    assert_eq!((monthly.web, monthly.ci), (1, 2));
}

#[tokio::test]
async fn the_orphan_cas_requires_stale_progress() {
    // progress after the 11:55 cutoff: not stale
    let (env, turn) = orphan_world(|chat| {
        TurnSeed::standard(1, chat).started("2026-03-10T11:00:00Z", Some("2026-03-10T11:56:00Z"))
    })
    .await;
    let quota = env.quota_dump().await;

    let result = env.repo.finalize_turn(&orphan_request(turn)).await.unwrap();

    assert_eq!(result, FinalizeResult::Lost);
    assert_eq!(env.turn_state(turn).await, "running");
    assert_eq!(env.quota_dump().await, quota);
    assert!(env.usage_rows().await.is_empty());
}

#[tokio::test]
async fn progress_exactly_at_the_cutoff_is_stale() {
    let (env, turn) = orphan_world(|chat| {
        TurnSeed::standard(1, chat).started("2026-03-09T23:59:50Z", Some("2026-03-10T11:55:00Z"))
    })
    .await;

    let result = env.repo.finalize_turn(&orphan_request(turn)).await.unwrap();

    assert!(matches!(result, FinalizeResult::Won(_)));
}

#[tokio::test]
async fn without_progress_the_start_time_is_compared_with_the_cutoff() {
    let (env, recent) =
        orphan_world(|chat| TurnSeed::standard(1, chat).started("2026-03-10T11:58:00Z", None))
            .await;
    assert_eq!(
        env.repo
            .finalize_turn(&orphan_request(recent))
            .await
            .unwrap(),
        FinalizeResult::Lost,
        "started after the cutoff"
    );

    let (env, old) = orphan_world(stale_seed).await;
    assert!(matches!(
        env.repo.finalize_turn(&orphan_request(old)).await.unwrap(),
        FinalizeResult::Won(_)
    ));
}

#[tokio::test]
async fn the_orphan_cas_skips_soft_deleted_and_already_terminal_turns() {
    let (env, deleted) = orphan_world(|chat| {
        let mut seed = stale_seed(chat);
        seed.deleted = true;
        seed
    })
    .await;
    assert_eq!(
        env.repo
            .finalize_turn(&orphan_request(deleted))
            .await
            .unwrap(),
        FinalizeResult::Lost
    );
    assert_eq!(env.turn_state(deleted).await, "running");

    let (env, done) = orphan_world(|chat| stale_seed(chat).state("completed")).await;
    assert_eq!(
        env.repo.finalize_turn(&orphan_request(done)).await.unwrap(),
        FinalizeResult::Lost
    );
}

#[tokio::test]
async fn a_stream_finalizer_that_wins_first_makes_the_watchdog_lose() {
    let (env, turn) = orphan_world(stale_seed).await;
    finalize(&env, &StreamReq::cancelled(turn)).await;
    let quota = env.quota_dump().await;

    let result = env.repo.finalize_turn(&orphan_request(turn)).await.unwrap();

    assert_eq!(result, FinalizeResult::Lost);
    assert_eq!(env.quota_dump().await, quota);
    assert_eq!(env.usage_rows().await.len(), 1);
}

// ================================================================================================
// Corrupt data
// ================================================================================================

#[tokio::test]
async fn a_turn_without_a_requester_is_finalized_without_settlement_event_or_cost_record() {
    for orphan in [false, true] {
        let capture = LogCapture::start();
        let env = QuotaEnv::new().await;
        let chat = env.seed_chat(1).await;
        let turn = env.seed_turn(&stale_seed(chat).without_requester()).await;
        env.seed_bucket(&BucketSeed::total("daily", 25_000_000, 1_500_000).at("2026-03-09"))
            .await;
        env.seed_bucket(&BucketSeed::total("daily", 25_000_000, 1_500_000))
            .await;
        env.seed_bucket(&BucketSeed::total("monthly", 240_000_000, 1_500_000))
            .await;
        let quota = env.quota_dump().await;

        let result = if orphan {
            env.repo.finalize_turn(&orphan_request(turn)).await.unwrap()
        } else {
            finalize(&env, &StreamReq::completed(turn).usage(900, 300)).await
        };

        let won = won(result);
        assert!(!won.settled, "orphan={orphan}");
        assert!(!won.usage_event_enqueued, "orphan={orphan}");
        assert_eq!(
            env.turn_state(turn).await,
            if orphan { "failed" } else { "completed" },
            "the CAS commits and the turn is finalized (orphan={orphan})"
        );
        assert_eq!(env.quota_dump().await, quota, "no quota_usage row changes");
        assert_eq!(env.outbox_message_count().await, 0, "no outbox message");
        assert!(
            !capture
                .at_containing(Level::ERROR, &turn.to_string())
                .is_empty(),
            "orphan={orphan}: {}",
            capture.all_text()
        );
        assert!(
            capture
                .at_containing(Level::INFO, "quota settled")
                .is_empty()
        );
        // the turn is not found by the next scan
        assert_eq!(
            env.repo.finalize_turn(&orphan_request(turn)).await.unwrap(),
            FinalizeResult::Lost
        );
    }
}

#[tokio::test]
async fn a_turn_without_reserve_fields_is_finalized_with_a_zero_credit_aborted_estimated_event() {
    let capture = LogCapture::start();
    let env = QuotaEnv::new().await;
    let chat = env.seed_chat(1).await;
    let turn = env
        .seed_turn(
            &TurnSeed::standard(1, chat)
                .without_reserve()
                .started("2026-03-09T23:59:50Z", None),
        )
        .await;
    env.seed_design_world(0, false).await;
    let quota = env.quota_dump().await;

    let result = env.repo.finalize_turn(&orphan_request(turn)).await.unwrap();

    let won = won(result);
    assert!(!won.settled);
    assert!(won.usage_event_enqueued);
    assert_eq!(won.committed_credits_micro, 0);
    assert_eq!(env.turn_state(turn).await, "failed");
    assert_eq!(env.quota_dump().await, quota, "no quota_usage row changes");
    assert!(
        !capture.at(Level::WARN).is_empty(),
        "{}",
        capture.all_text()
    );
    let (event, raw, _) = only_event(&env).await;
    assert_eq!(event.billing_outcome, BillingOutcome::Aborted);
    assert_eq!(event.settlement_method, SettlementMethod::Estimated);
    assert_eq!(event.terminal_state, TerminalState::Failed);
    assert_eq!(event.actual_credits_micro, 0);
    assert!(raw["usage"].is_null());
    assert_eq!(event.effective_model, "");
    assert_eq!(event.selected_model, "");
    assert_eq!(event.policy_version_applied, 0);
}

#[tokio::test]
async fn a_stream_finalization_of_a_turn_without_reserve_fields_also_skips_the_settlement() {
    let env = QuotaEnv::new().await;
    let chat = env.seed_chat(1).await;
    let turn = env
        .seed_turn(&TurnSeed::standard(1, chat).without_reserve())
        .await;
    env.seed_design_world(0, false).await;
    let quota = env.quota_dump().await;

    let result = finalize(&env, &StreamReq::cancelled(turn)).await;

    assert!(!won(result).settled);
    assert_eq!(env.quota_dump().await, quota);
    let (event, _, _) = only_event(&env).await;
    assert_eq!(
        (
            event.effective_model.as_str(),
            event.selected_model.as_str()
        ),
        ("", "")
    );
    assert_eq!(event.actual_credits_micro, 0);
    assert_eq!(event.settlement_method, SettlementMethod::Estimated);
}

// ================================================================================================
// Rows created at settlement
// ================================================================================================

#[tokio::test]
async fn a_missing_bucket_row_is_created_with_the_deltas_applied_to_zero() {
    let env = QuotaEnv::new().await;
    let chat = env.seed_chat(1).await;
    let turn = env.seed_turn(&TurnSeed::standard(1, chat)).await;
    // no quota_usage rows at all

    let result = finalize(
        &env,
        &StreamReq::completed(turn).usage(900, 300).tools(1, 0, 0),
    )
    .await;

    assert!(matches!(result, FinalizeResult::Won(_)));
    assert_eq!(
        env.turn_state(turn).await,
        "completed",
        "the finalization does not fail"
    );
    for period in ["daily", "monthly"] {
        let row = env.total(period).await.expect(period);
        assert_eq!(row.spent, 1_200_000, "{period}");
        assert_eq!(
            row.reserved, -1_500_000,
            "{period}: the release applied to zero"
        );
        assert_eq!(
            (row.calls, row.input_tokens, row.output_tokens, row.web),
            (1, 900, 300, 1),
            "{period}"
        );
        assert_eq!(row.updated_at, NOON_TEXT);
    }
    assert_eq!(env.bucket_count().await, 2);
}

#[tokio::test]
async fn settlement_only_touches_the_rows_of_the_turns_user() {
    let (env, turn) = standard_world().await;
    let other = Uuid::from_u128(0x56);
    env.seed_bucket(&BucketSeed::total("daily", 7, 7).of(USER_TENANT, other))
        .await;

    finalize(&env, &StreamReq::completed(turn).usage(900, 300)).await;

    let untouched = env
        .bucket_of(USER_TENANT, other, "daily", "2026-03-10", "total")
        .await
        .unwrap();
    assert_eq!(
        (untouched.spent, untouched.reserved, untouched.calls),
        (7, 7, 0)
    );
}

// ================================================================================================
// Write-first
// ================================================================================================

#[tokio::test]
async fn a_finalization_waits_for_a_competing_writer_instead_of_failing_because_its_first_statement_is_the_cas()
 {
    use sea_orm::ConnectionTrait;
    let env = QuotaEnv::file().await;
    let (env, turn) = standard_world_in(env).await;
    let request = StreamReq::completed(turn).usage(900, 300).build();

    // Held for 400 ms, longer than the whole conflict backoff.
    let (result, ()) = tokio::join!(
        async {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            env.repo.finalize_turn(&request).await
        },
        async {
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
            tokio::time::sleep(std::time::Duration::from_millis(400)).await;
            env.db.raw.execute_unprepared("COMMIT").await.unwrap();
        }
    );

    assert!(matches!(result, Ok(FinalizeResult::Won(_))), "{result:?}");
    assert_eq!(env.turn_state(turn).await, "completed");
    assert_eq!(env.usage_rows().await.len(), 1);
}

// ================================================================================================
// Delivery and mixed concurrency
// ================================================================================================

#[tokio::test]
async fn the_plugin_receives_the_event_of_the_finalized_turn_through_the_usage_handler() {
    let env = QuotaEnv::with(Options {
        deliver_usage: true,
        ..Options::default()
    })
    .await;
    let (env, turn) = standard_world_in(env).await;

    finalize(
        &env,
        &StreamReq::completed(turn).usage(900, 300).tools(1, 0, 0),
    )
    .await;

    let plugin = env.plugin.as_ref().unwrap();
    let mut published = Vec::new();
    for _ in 0..200 {
        published = plugin.published_events();
        if !published.is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert_eq!(
        published.len(),
        1,
        "the usage message is delivered exactly once"
    );
    let event = &published[0];
    assert_eq!(event.turn_id, Some(turn));
    assert_eq!(event.actual_credits_micro, 1_200_000);
    assert_eq!(event.web_search_calls, 1);
    assert_eq!(
        event.dedupe_key,
        usage_dedupe_key(USER_TENANT, turn, request_id(1))
    );
}

#[tokio::test]
async fn a_finalization_and_a_reserve_of_the_same_user_run_concurrently_without_locking_errors() {
    use crate::domain::service::quota::reserve::{ReserveMode, ReserveRequest};
    use crate::domain::service::quota::test_support::standard_decision;
    let env = QuotaEnv::file().await;
    let (env, turn) = standard_world_in(env).await;
    let decision = standard_decision();
    let reserve = ReserveRequest {
        tenant_id: USER_TENANT,
        user_id: USER,
        mode: ReserveMode::Send,
        decision: &decision,
        now: noon(),
    };
    let request = StreamReq::completed(turn).usage(900, 300).build();

    let (finalized, reserved) =
        tokio::join!(env.repo.finalize_turn(&request), env.repo.reserve(&reserve));

    assert!(
        matches!(finalized, Ok(FinalizeResult::Won(_))),
        "{finalized:?}"
    );
    assert!(reserved.is_ok(), "{reserved:?}");
    let daily = env.total("daily").await.unwrap();
    assert_eq!(daily.spent, 26_200_000);
    assert_eq!(
        daily.reserved, 1_500_000,
        "only the new reserve is left in flight"
    );
}
