#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Slice 4 and 5: enqueueing the usage events in the caller's transaction
//! (`cpt-cf-mini-chat-algo-quota-billing-turn-usage-event`,
//! `cpt-cf-mini-chat-algo-quota-billing-system-task-usage`): queue, partition,
//! payload, atomicity with the caller's other writes, and the system-task
//! helper that never touches `quota_usage` or `chat_turns`.

use std::sync::Arc;

use mini_chat_sdk::{
    BillingOutcome, SettlementMethod, SystemTaskType, TerminalState, UsageEvent, UsageTokens,
    decode_usage_event, encode_usage_event, system_task_dedupe_key,
};
use serde_json::Value;
use uuid::Uuid;

use super::{enqueue_system_task_usage, enqueue_usage_event};
use crate::domain::error::DomainError;
use crate::domain::service::quota::events::{
    SystemTaskUsageArgs, TurnUsageEventArgs, build_system_task_event, build_turn_usage_event,
};
use crate::domain::service::quota::settle::ToolCallCounts;
use crate::domain::service::quota::test_support::noon;
use crate::infra::outbox::queues::partition_for;
use crate::quota_test_env::{BucketSeed, Options, QuotaEnv, TurnSeed};
use crate::test_doubles::{OTHER_TENANT, USER, USER_TENANT};

fn turn_event(tenant: Uuid, chat: Uuid) -> UsageEvent {
    build_turn_usage_event(&TurnUsageEventArgs {
        tenant_id: tenant,
        user_id: USER,
        chat_id: chat,
        turn_id: Uuid::from_u128(0xA1),
        request_id: Uuid::from_u128(0xE1),
        effective_model: "s".to_owned(),
        selected_model: "p".to_owned(),
        terminal_state: TerminalState::Completed,
        billing_outcome: BillingOutcome::Completed,
        settlement_method: SettlementMethod::Actual,
        committed_credits_micro: 1_200_000,
        policy_version_applied: 7,
        usage: Some(UsageTokens {
            input_tokens: 900,
            output_tokens: 300,
            cache_read_input_tokens: 0,
            cache_write_input_tokens: 0,
            reasoning_tokens: 0,
        }),
        tool_calls: ToolCallCounts::default(),
        now: noon(),
    })
}

fn system_args(request: Uuid) -> SystemTaskUsageArgs {
    SystemTaskUsageArgs {
        tenant_id: USER_TENANT,
        chat_id: Uuid::from_u128(0xC1),
        system_request_id: request,
        summary_model: "summary-model".to_owned(),
        usage: Some(UsageTokens {
            input_tokens: 2000,
            output_tokens: 150,
            cache_read_input_tokens: 0,
            cache_write_input_tokens: 0,
            reasoning_tokens: 0,
        }),
        now: noon(),
    }
}

async fn enqueue(env: &QuotaEnv, event: UsageEvent) -> Result<(), DomainError> {
    let enqueuer = Arc::clone(&env.enqueuer);
    env.provider
        .transaction(move |tx| {
            Box::pin(async move { enqueue_usage_event(tx, &enqueuer, &event).await })
        })
        .await
        .map(toolkit_db::outbox::Wake::fire)
}

async fn enqueue_system(env: &QuotaEnv, args: SystemTaskUsageArgs) -> Result<(), DomainError> {
    let enqueuer = Arc::clone(&env.enqueuer);
    env.provider
        .transaction(move |tx| {
            Box::pin(async move { enqueue_system_task_usage(tx, &enqueuer, &args).await })
        })
        .await
        .map(toolkit_db::outbox::Wake::fire)
}

// ---- the user event ----------------------------------------------------------------------------------

#[tokio::test]
async fn a_user_event_is_stored_in_the_usage_queue_with_the_encoded_payload() {
    let env = QuotaEnv::new().await;
    let event = turn_event(USER_TENANT, Uuid::from_u128(0xC1));

    enqueue(&env, event.clone()).await.unwrap();

    let rows = env.usage_rows().await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].queue, "mini-chat.usage_snapshot");
    assert_eq!(rows[0].payload, encode_usage_event(&event).unwrap());
    assert_eq!(decode_usage_event(&rows[0].payload).unwrap(), event);
}

#[tokio::test]
async fn the_partition_is_chosen_by_the_tenant_not_by_the_chat() {
    let env = QuotaEnv::new().await;
    enqueue(&env, turn_event(USER_TENANT, Uuid::from_u128(0xC1)))
        .await
        .unwrap();
    enqueue(&env, turn_event(USER_TENANT, Uuid::from_u128(0xC2)))
        .await
        .unwrap();
    enqueue(&env, turn_event(OTHER_TENANT, Uuid::from_u128(0xC1)))
        .await
        .unwrap();

    let rows = env.usage_rows().await;

    assert_eq!(rows.len(), 3);
    let partitions: Vec<i64> = rows.iter().map(|r| r.partition).collect();
    let tenant_partition = i64::from(partition_for(USER_TENANT, 4));
    let other_partition = i64::from(partition_for(OTHER_TENANT, 4));
    assert_eq!(
        partitions
            .iter()
            .filter(|p| **p == tenant_partition)
            .count(),
        if tenant_partition == other_partition {
            3
        } else {
            2
        }
    );
    assert!(partitions.contains(&other_partition));
}

#[tokio::test]
async fn the_configured_queue_name_is_used() {
    let env = QuotaEnv::with(Options {
        usage_queue: Some("custom.usage"),
        ..Options::default()
    })
    .await;

    enqueue(&env, turn_event(USER_TENANT, Uuid::from_u128(0xC1)))
        .await
        .unwrap();

    assert_eq!(env.outbox_rows("custom.usage").await.len(), 1);
    assert!(env.outbox_rows("mini-chat.usage_snapshot").await.is_empty());
}

#[tokio::test]
async fn a_failure_after_the_enqueue_rolls_the_message_back() {
    let env = QuotaEnv::new().await;
    let enqueuer = Arc::clone(&env.enqueuer);
    let event = turn_event(USER_TENANT, Uuid::from_u128(0xC1));

    let result = env
        .provider
        .transaction(move |tx| {
            Box::pin(async move {
                let _wake = enqueue_usage_event(tx, &enqueuer, &event).await?;
                Err::<(), _>(DomainError::Internal("a later step failed".to_owned()))
            })
        })
        .await;

    assert!(matches!(result, Err(DomainError::Internal(_))));
    assert!(env.usage_rows().await.is_empty());
    assert_eq!(env.outbox_message_count().await, 0);
}

#[tokio::test]
async fn a_user_event_without_a_user_id_cannot_be_encoded_and_enqueues_nothing() {
    let env = QuotaEnv::new().await;
    let mut event = turn_event(USER_TENANT, Uuid::from_u128(0xC1));
    event.user_id = None;

    let result = enqueue(&env, event).await;

    assert!(
        matches!(result, Err(DomainError::Internal(_))),
        "{result:?}"
    );
    assert_eq!(env.outbox_message_count().await, 0);
}

#[tokio::test]
async fn without_a_running_outbox_the_enqueue_is_an_internal_error() {
    let env = QuotaEnv::with(Options {
        outbox: false,
        ..Options::default()
    })
    .await;

    let result = enqueue(&env, turn_event(USER_TENANT, Uuid::from_u128(0xC1))).await;

    assert!(
        matches!(result, Err(DomainError::Internal(_))),
        "{result:?}"
    );
}

// ---- the system-task helper ---------------------------------------------------------------------------

#[tokio::test]
async fn a_thread_summary_commit_enqueues_one_system_task_event_with_the_fixed_fields() {
    let env = QuotaEnv::new().await;
    let request = Uuid::from_u128(0x5151);

    enqueue_system(&env, system_args(request)).await.unwrap();

    let rows = env.usage_rows().await;
    assert_eq!(rows.len(), 1);
    let raw: Value = rows[0].json();
    let object = raw.as_object().unwrap();
    assert!(!object.contains_key("user_id"), "absent, not null");
    assert!(!object.contains_key("turn_id"));
    assert_eq!(raw["requester_type"], "system");
    assert_eq!(raw["system_task_type"], "thread_summary_update");
    assert_eq!(raw["billing_outcome"], "system_task");
    assert_eq!(raw["settlement_method"], "none");
    assert_eq!(raw["terminal_state"], "completed");
    assert_eq!(raw["actual_credits_micro"], 0);
    assert_eq!(raw["policy_version_applied"], 0);
    assert_eq!(raw["effective_model"], "summary-model");
    assert_eq!(raw["selected_model"], "summary-model");
    assert_eq!(
        raw["dedupe_key"],
        system_task_dedupe_key(USER_TENANT, SystemTaskType::ThreadSummaryUpdate, request)
    );
    let event = decode_usage_event(&rows[0].payload).unwrap();
    assert_eq!(event, build_system_task_event(&system_args(request)));
    assert_eq!(rows[0].partition, i64::from(partition_for(USER_TENANT, 4)));
}

#[tokio::test]
async fn the_system_task_helper_never_reads_or_writes_quota_or_turns() {
    let env = QuotaEnv::new().await;
    let chat = env.seed_chat(1).await;
    env.seed_turn(&TurnSeed::standard(1, chat)).await;
    env.seed_bucket(&BucketSeed::total("daily", 5, 6)).await;
    let quota = env.quota_dump().await;
    let turns = env.dump("chat_turns").await;

    enqueue_system(&env, system_args(Uuid::from_u128(0x5151)))
        .await
        .unwrap();

    assert_eq!(env.quota_dump().await, quota, "no user bucket is debited");
    assert_eq!(
        env.dump("chat_turns").await,
        turns,
        "no chat_turns row is created or CASed"
    );
    assert_eq!(env.usage_rows().await.len(), 1);
}

#[tokio::test]
async fn a_retry_of_the_same_system_request_carries_the_same_dedupe_key() {
    let env = QuotaEnv::new().await;
    let request = Uuid::from_u128(0x5151);

    enqueue_system(&env, system_args(request)).await.unwrap();
    enqueue_system(&env, system_args(request)).await.unwrap();

    let rows = env.usage_rows().await;
    assert_eq!(rows.len(), 2, "at-least-once: the consumer deduplicates");
    let keys: Vec<String> = rows
        .iter()
        .map(|row| decode_usage_event(&row.payload).unwrap().dedupe_key)
        .collect();
    assert_eq!(keys[0], keys[1]);
}

#[tokio::test]
async fn the_system_task_event_is_atomic_with_the_summary_upsert_of_the_caller() {
    let env = QuotaEnv::new().await;
    // control: a committed transaction keeps its message
    enqueue_system(&env, system_args(Uuid::from_u128(0x5150)))
        .await
        .unwrap();
    let enqueuer = Arc::clone(&env.enqueuer);
    let args = system_args(Uuid::from_u128(0x5151));

    let result = env
        .provider
        .transaction(move |tx| {
            Box::pin(async move {
                let _wake = enqueue_system_task_usage(tx, &enqueuer, &args).await?;
                Err::<(), _>(DomainError::Internal(
                    "the summary upsert failed".to_owned(),
                ))
            })
        })
        .await;

    assert!(result.is_err());
    assert_eq!(
        env.usage_rows().await.len(),
        1,
        "only the committed control message exists"
    );
}

#[tokio::test]
async fn without_a_running_outbox_the_system_task_helper_is_an_internal_error() {
    // control: the same call succeeds when the outbox runs
    let running = QuotaEnv::new().await;
    enqueue_system(&running, system_args(Uuid::from_u128(0x5151)))
        .await
        .unwrap();
    let env = QuotaEnv::with(Options {
        outbox: false,
        ..Options::default()
    })
    .await;

    let result = enqueue_system(&env, system_args(Uuid::from_u128(0x5151))).await;

    assert!(
        matches!(result, Err(DomainError::Internal(_))),
        "{result:?}"
    );
}

// ---- database failures keep their contention text ---------------------------------------------------

/// A trigger that makes every outbox body insert fail like a busy `SQLite`
/// database (the text carries the `(code: 5)` / `database is locked` markers
/// the contention classifier matches).
async fn fail_outbox_insert_with_contention(env: &QuotaEnv) {
    use sea_orm::ConnectionTrait;
    env.db
        .raw
        .execute_unprepared(
            "CREATE TRIGGER outbox_busy BEFORE INSERT ON mini_chat__outbox_body \
             BEGIN SELECT RAISE(ABORT, '(code: 5) database is locked'); END",
        )
        .await
        .expect("create trigger");
}

fn assert_contention(result: Result<(), DomainError>) {
    let Err(DomainError::Database(message)) = result else {
        panic!("a database failure of the enqueue must be DomainError::Database: {result:?}");
    };
    assert!(
        toolkit_db::contention::is_retryable_contention(
            sea_orm::DbBackend::Sqlite,
            &sea_orm::DbErr::Custom(message.clone()),
        ),
        "the contention text is preserved: {message}"
    );
}

#[tokio::test]
async fn a_database_contention_at_the_enqueue_is_a_retryable_database_error() {
    let env = QuotaEnv::new().await;
    fail_outbox_insert_with_contention(&env).await;

    let result = enqueue(&env, turn_event(USER_TENANT, Uuid::from_u128(0xC1))).await;

    assert_contention(result);
    assert_eq!(env.outbox_message_count().await, 0);
}

#[tokio::test]
async fn a_database_contention_at_the_system_task_enqueue_is_a_retryable_database_error() {
    let env = QuotaEnv::new().await;
    fail_outbox_insert_with_contention(&env).await;

    let result = enqueue_system(&env, system_args(Uuid::from_u128(0x5151))).await;

    assert_contention(result);
    assert_eq!(env.outbox_message_count().await, 0);
}
