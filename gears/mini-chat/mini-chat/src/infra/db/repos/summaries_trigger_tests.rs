#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Slice 4: the statements of the thread-summary trigger inside a transaction
//! (`cpt-cf-mini-chat-algo-background-workers-evaluate-summary-trigger`,
//! `inst-read-base-frontier` to `inst-return-trigger-result`): the base
//! frontier re-read in the transaction, the frozen target (the latest live
//! message outside the causing turn), `not_needed` for no earlier message and a
//! frontier at the target, and the enqueue partitioned by `chat_id`.

use std::sync::Arc;

use toolkit_db::outbox::Wake;
use uuid::Uuid;

use super::evaluate_summary_trigger;
use crate::chat_test_env::{blob, request_id};
use crate::domain::error::DomainError;
use crate::domain::service::workers::summary_trigger::{
    SummaryTrigger, SummaryTriggerPlan, TriggerResult,
};
use crate::infra::outbox::queues::{OutboxEnqueuer, QueueKind};
use crate::mutation_test_env::SeedTurn;
use crate::test_doubles::USER_TENANT;
use crate::worker_test_env::{WorkerEnv, at};

const SYSTEM_REQUEST: Uuid = Uuid::from_u128(0x5157_0001);

fn plan() -> SummaryTriggerPlan {
    SummaryTriggerPlan {
        enabled: true,
        compression_threshold_pct: 80,
        assembled_context_tokens: 900,
        messages_truncated: false,
        effective_input_budget: 1000,
        summary_applied: false,
        system_request_id: SYSTEM_REQUEST,
    }
}

fn trigger(chat: Uuid, causing: Uuid, plan: SummaryTriggerPlan) -> SummaryTrigger {
    SummaryTrigger {
        plan,
        completed: true,
        tenant_id: USER_TENANT,
        chat_id: chat,
        causing_request_id: causing,
    }
}

/// Runs the trigger in its own transaction and fires the wake after it.
async fn run(
    env: &WorkerEnv,
    enqueuer: &Arc<OutboxEnqueuer>,
    trigger: &SummaryTrigger,
) -> Result<TriggerResult, DomainError> {
    let (enqueuer, trigger) = (Arc::clone(enqueuer), trigger.clone());
    let (result, wake): (TriggerResult, Option<Wake>) = env
        .quota
        .provider
        .transaction(move |tx| {
            Box::pin(async move { evaluate_summary_trigger(tx, &enqueuer, &trigger).await })
        })
        .await?;
    if let Some(wake) = wake {
        wake.fire();
    }
    Ok(result)
}

async fn evaluate(env: &WorkerEnv, trigger: &SummaryTrigger) -> TriggerResult {
    run(env, &env.quota.enqueuer, trigger).await.unwrap()
}

#[tokio::test]
async fn a_first_summary_is_scheduled_with_a_null_base_and_the_latest_message_before_the_causing_turn()
 {
    let env = WorkerEnv::new().await;
    let turns = env.conversation(3).await; // the causing turn is turn 3
    let causing = SeedTurn::completed(3, 30).request_id();

    let result = evaluate(&env, &trigger(env.chat, causing, plan())).await;

    assert_eq!(result, TriggerResult::Scheduled);
    let items = env.summary_messages().await;
    assert_eq!(items.len(), 1);
    let item = &items[0];
    assert!(
        item["base_frontier_created_at"].is_null() && item["base_frontier_message_id"].is_null()
    );
    assert_eq!(
        item["frozen_target_message_id"],
        turns[1].assistant_message.to_string()
    );
    assert_eq!(item["system_request_id"], SYSTEM_REQUEST.to_string());
    assert_eq!(item["tenant_id"], USER_TENANT.to_string());
    assert_eq!(item["chat_id"], env.chat.to_string());
    assert_eq!(item["system_task_type"], "thread_summary_update");
    assert_eq!(
        item["frozen_target_created_at"],
        "2024-01-01T00:00:21.000000Z"
    );
}

#[tokio::test]
async fn the_causing_turn_is_never_part_of_the_frozen_range() {
    let env = WorkerEnv::new().await;
    env.conversation(2).await;
    // the causing turn's messages are the newest of the chat
    let causing = SeedTurn::completed(2, 20).request_id();

    evaluate(&env, &trigger(env.chat, causing, plan())).await;

    let item = &env.summary_messages().await[0];
    assert_eq!(
        item["frozen_target_message_id"],
        SeedTurn::completed(1, 10)
            .assistant_message_id()
            .to_string(),
        "the target is the answer of turn 1"
    );
}

#[tokio::test]
async fn a_message_without_a_request_id_is_not_the_causing_turn_and_can_be_the_target() {
    let env = WorkerEnv::new().await;
    let turns = env.conversation(3).await;
    // the newest message before the causing turn has a NULL `request_id`; a bare
    // `request_id <> :causing` would drop it (SQL three-valued logic)
    env.exec(&format!(
        "UPDATE messages SET request_id = NULL WHERE id = {}",
        blob(turns[1].assistant_message)
    ))
    .await;
    let causing = SeedTurn::completed(3, 30).request_id();

    let result = evaluate(&env, &trigger(env.chat, causing, plan())).await;

    assert_eq!(result, TriggerResult::Scheduled);
    let item = &env.summary_messages().await[0];
    assert_eq!(
        item["frozen_target_message_id"],
        turns[1].assistant_message.to_string(),
        "the NULL-request message is selected, not skipped"
    );
}

#[tokio::test]
async fn soft_deleted_messages_are_not_a_target() {
    let env = WorkerEnv::new().await;
    env.conversation(3).await;
    let turn2 = SeedTurn::completed(2, 20);
    env.m
        .quota
        .db
        .soft_delete("messages", turn2.assistant_message_id())
        .await;
    env.m
        .quota
        .db
        .soft_delete("messages", turn2.user_message_id())
        .await;
    let causing = SeedTurn::completed(3, 30).request_id();

    evaluate(&env, &trigger(env.chat, causing, plan())).await;

    let item = &env.summary_messages().await[0];
    assert_eq!(
        item["frozen_target_message_id"],
        SeedTurn::completed(1, 10)
            .assistant_message_id()
            .to_string()
    );
}

#[tokio::test]
async fn the_base_frontier_is_the_stored_one_read_in_the_transaction() {
    let env = WorkerEnv::new().await;
    let turns = env.conversation(3).await;
    env.seed_summary(env.chat, "earlier", turns[0].assistant_message, 11, 7)
        .await;
    let mut urgent = plan();
    urgent.messages_truncated = true;
    urgent.summary_applied = true;
    let causing = SeedTurn::completed(3, 30).request_id();

    let result = evaluate(&env, &trigger(env.chat, causing, urgent)).await;

    assert_eq!(result, TriggerResult::Scheduled);
    let item = &env.summary_messages().await[0];
    assert_eq!(
        item["base_frontier_message_id"],
        turns[0].assistant_message.to_string()
    );
    assert_eq!(
        item["base_frontier_created_at"],
        "2024-01-01T00:00:11.000000Z"
    );
    assert_eq!(
        item["frozen_target_message_id"],
        turns[1].assistant_message.to_string()
    );
}

#[tokio::test]
async fn a_frontier_already_at_the_target_is_not_needed_and_enqueues_nothing() {
    let env = WorkerEnv::new().await;
    let turns = env.conversation(3).await;
    env.seed_summary(env.chat, "earlier", turns[1].assistant_message, 21, 7)
        .await;
    let mut urgent = plan();
    urgent.messages_truncated = true;
    let causing = SeedTurn::completed(3, 30).request_id();

    let result = evaluate(&env, &trigger(env.chat, causing, urgent)).await;

    assert_eq!(result, TriggerResult::NotNeeded);
    assert!(env.summary_messages().await.is_empty());
}

#[tokio::test]
async fn a_frontier_after_the_target_is_not_needed() {
    let env = WorkerEnv::new().await;
    let turns = env.conversation(3).await;
    env.seed_summary(env.chat, "later", turns[2].assistant_message, 31, 7)
        .await;
    let mut urgent = plan();
    urgent.messages_truncated = true;
    let causing = SeedTurn::completed(3, 30).request_id();

    assert_eq!(
        evaluate(&env, &trigger(env.chat, causing, urgent)).await,
        TriggerResult::NotNeeded
    );
}

#[tokio::test]
async fn a_chat_without_an_earlier_message_is_not_needed() {
    let env = WorkerEnv::new().await;
    env.conversation(1).await;
    let causing = SeedTurn::completed(1, 10).request_id();

    let result = evaluate(&env, &trigger(env.chat, causing, plan())).await;

    assert_eq!(result, TriggerResult::NotNeeded);
    assert!(env.summary_messages().await.is_empty());
}

#[tokio::test]
async fn an_empty_chat_is_not_needed() {
    let env = WorkerEnv::new().await;

    assert_eq!(
        evaluate(&env, &trigger(env.chat, request_id(9), plan())).await,
        TriggerResult::NotNeeded
    );
}

#[tokio::test]
async fn a_summary_that_is_effective_and_a_turn_without_truncation_is_not_needed_without_a_read() {
    let env = WorkerEnv::new().await;
    env.conversation(3).await;
    let mut summarized = plan();
    summarized.summary_applied = true;
    let causing = SeedTurn::completed(3, 30).request_id();

    let result = evaluate(&env, &trigger(env.chat, causing, summarized)).await;

    assert_eq!(result, TriggerResult::NotNeeded);
    assert!(env.summary_messages().await.is_empty());
}

#[tokio::test]
async fn a_trigger_that_is_not_evaluated_touches_no_table() {
    let env = WorkerEnv::new().await;
    env.conversation(3).await;
    let causing = SeedTurn::completed(3, 30).request_id();
    let mut off = plan();
    off.enabled = false;
    let mut below = plan();
    below.assembled_context_tokens = 10;
    let mut not_completed = trigger(env.chat, causing, plan());
    not_completed.completed = false;

    for trigger in [
        trigger(env.chat, causing, off),
        trigger(env.chat, causing, below),
        not_completed,
    ] {
        assert_eq!(evaluate(&env, &trigger).await, TriggerResult::NotEvaluated);
    }

    assert!(env.summary_messages().await.is_empty());
}

#[tokio::test]
async fn the_work_item_is_partitioned_by_the_chat_id() {
    let env = WorkerEnv::new().await;
    env.conversation(3).await;
    let causing = SeedTurn::completed(3, 30).request_id();

    evaluate(&env, &trigger(env.chat, causing, plan())).await;

    let rows = env
        .quota
        .outbox_rows(crate::worker_test_env::SUMMARY_QUEUE)
        .await;
    assert_eq!(rows.len(), 1);
    assert_eq!(
        u32::try_from(rows[0].partition).unwrap(),
        env.quota
            .enqueuer
            .partition(QueueKind::ThreadSummary, USER_TENANT, env.chat),
        "by chat_id"
    );
}

#[tokio::test]
async fn messages_of_another_chat_are_never_a_target() {
    let env = WorkerEnv::new().await;
    env.conversation(2).await;
    let other = env.quota.seed_chat(2).await;
    env.m.seed_turn_in(other, &SeedTurn::completed(7, 90)).await;
    let causing = SeedTurn::completed(2, 20).request_id();

    evaluate(&env, &trigger(env.chat, causing, plan())).await;

    let item = &env.summary_messages().await[0];
    assert_eq!(
        item["frozen_target_message_id"],
        SeedTurn::completed(1, 10)
            .assistant_message_id()
            .to_string()
    );
}

#[tokio::test]
async fn an_enqueue_failure_fails_the_transaction_like_any_statement_of_the_closure() {
    let env = WorkerEnv::new().await;
    env.conversation(3).await;
    let causing = SeedTurn::completed(3, 30).request_id();
    let mismatched = Arc::new(OutboxEnqueuer::new(&crate::config::OutboxConfig {
        thread_summary_queue_name: "mini-chat.not_registered".to_owned(),
        num_partitions: 4,
        ..crate::config::OutboxConfig::default()
    }));
    assert!(mismatched.bind(&env.quota.enqueuer.outbox().unwrap()));

    let result = run(&env, &mismatched, &trigger(env.chat, causing, plan())).await;

    assert!(result.is_err(), "{result:?}");
    assert!(env.summary_messages().await.is_empty());
}

#[test]
fn the_helper_boundary_matches_the_fixture_epoch() {
    assert_eq!(at(0).unix_timestamp(), 1_704_067_200);
}
