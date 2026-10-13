#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Slice 5: the Secure ORM summary store and its guarded commit
//! (`cpt-cf-mini-chat-algo-background-workers-commit-summary`,
//! `cpt-cf-mini-chat-dod-background-workers-summary-commit`,
//! `cpt-cf-mini-chat-state-background-workers-summary-frontier`): the reads, the
//! frontier insert and CAS, the frozen-target lock and re-check, the exact
//! `is_compressed` range and the system-task usage event in the same transaction.

use mini_chat_sdk::{BillingOutcome, SettlementMethod, UsageTokens};
use uuid::Uuid;

use super::SeaSummaryStore;
use crate::chat_test_env::{blob, ts};
use crate::domain::service::stream::ports::MessageBoundary;
use crate::domain::service::workers::ports::{CommitOutcome, SummaryCommit, SummaryStore};
use crate::mutation_test_env::{SeedTurn, Seeded};
use crate::test_doubles::{OTHER_TENANT, USER_TENANT};
use crate::worker_test_env::{WorkerEnv, at, boundary};

fn store(env: &WorkerEnv) -> SeaSummaryStore {
    SeaSummaryStore::new(
        std::sync::Arc::clone(&env.quota.provider),
        std::sync::Arc::clone(&env.quota.enqueuer),
    )
}

fn usage() -> UsageTokens {
    UsageTokens {
        input_tokens: 200,
        output_tokens: 40,
        cache_read_input_tokens: 5,
        cache_write_input_tokens: 0,
        reasoning_tokens: 10,
    }
}

fn commit(
    env: &WorkerEnv,
    base: Option<MessageBoundary>,
    target: MessageBoundary,
) -> SummaryCommit {
    SummaryCommit {
        tenant_id: USER_TENANT,
        chat_id: env.chat,
        base,
        target,
        summary_text: "the committed summary".to_owned(),
        token_estimate: 30,
        summary_id: Uuid::from_u128(0x5E),
        system_request_id: Uuid::from_u128(0x5157),
        summary_model: "sum".to_owned(),
        usage: Some(usage()),
        now: env.now(),
    }
}

async fn four(env: &WorkerEnv) -> Vec<Seeded> {
    env.conversation(4).await
}

// ---- the reads -----------------------------------------------------------------------------------------------------

#[tokio::test]
async fn the_reads_are_bound_to_the_tenant_and_the_chat() {
    let env = WorkerEnv::new().await;
    let turns = four(&env).await;
    env.seed_summary(env.chat, "text", turns[1].assistant_message, 21, 9)
        .await;
    let store = store(&env);

    let summary = store
        .summary(USER_TENANT, env.chat)
        .await
        .unwrap()
        .expect("a summary");
    assert_eq!(summary.text, "text");
    assert_eq!(summary.frontier, boundary(turns[1].assistant_message, 21));
    assert!(
        store
            .summary(OTHER_TENANT, env.chat)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        store
            .summary(USER_TENANT, Uuid::from_u128(0xDEAD))
            .await
            .unwrap()
            .is_none()
    );

    assert_eq!(
        store.chat_deleted(USER_TENANT, env.chat).await.unwrap(),
        Some(false)
    );
    assert_eq!(
        store.chat_deleted(OTHER_TENANT, env.chat).await.unwrap(),
        None
    );
    env.soft_delete_chat(env.chat).await;
    assert_eq!(
        store.chat_deleted(USER_TENANT, env.chat).await.unwrap(),
        Some(true)
    );

    let message = turns[0].user_message;
    assert_eq!(
        store
            .message_deleted(USER_TENANT, env.chat, message)
            .await
            .unwrap(),
        Some(false)
    );
    assert_eq!(
        store
            .message_deleted(OTHER_TENANT, env.chat, message)
            .await
            .unwrap(),
        None
    );
    assert_eq!(
        store
            .message_deleted(USER_TENANT, Uuid::from_u128(1), message)
            .await
            .unwrap(),
        None
    );
    env.m.quota.db.soft_delete("messages", message).await;
    assert_eq!(
        store
            .message_deleted(USER_TENANT, env.chat, message)
            .await
            .unwrap(),
        Some(true)
    );
}

#[tokio::test]
async fn the_range_is_live_non_compressed_messages_in_the_half_open_frontier_interval_in_order() {
    let env = WorkerEnv::new().await;
    let turns = four(&env).await;
    // turn 1 compressed, one message of turn 3 soft-deleted
    env.exec(&format!(
        "UPDATE messages SET is_compressed = 1 WHERE id = {}",
        blob(turns[0].user_message)
    ))
    .await;
    env.m
        .quota
        .db
        .soft_delete("messages", turns[2].user_message)
        .await;

    let range = store(&env)
        .range(
            USER_TENANT,
            env.chat,
            Some(boundary(turns[0].assistant_message, 11)),
            boundary(turns[2].assistant_message, 31),
        )
        .await
        .unwrap();

    let ids: Vec<Uuid> = range.iter().map(|m| m.id).collect();
    assert_eq!(
        ids,
        vec![
            turns[1].user_message,
            turns[1].assistant_message,
            turns[2].assistant_message
        ],
        "after the base (exclusive), up to the target (inclusive), live and not compressed"
    );
    assert_eq!(range[0].role, "user");
    assert_eq!(range[0].content, "question 2");
    assert_eq!(range[1].created_at, at(21));
}

#[tokio::test]
async fn without_a_base_the_range_starts_at_the_first_message_and_never_passes_the_target() {
    let env = WorkerEnv::new().await;
    let turns = four(&env).await;

    let range = store(&env)
        .range(
            USER_TENANT,
            env.chat,
            None,
            boundary(turns[1].user_message, 20),
        )
        .await
        .unwrap();

    let ids: Vec<Uuid> = range.iter().map(|m| m.id).collect();
    assert_eq!(
        ids,
        vec![
            turns[0].user_message,
            turns[0].assistant_message,
            turns[1].user_message
        ]
    );
}

#[tokio::test]
async fn the_range_of_another_tenant_or_chat_is_empty() {
    let env = WorkerEnv::new().await;
    let turns = four(&env).await;
    let target = boundary(turns[3].assistant_message, 41);

    assert!(
        store(&env)
            .range(OTHER_TENANT, env.chat, None, target)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        store(&env)
            .range(USER_TENANT, Uuid::from_u128(7), None, target)
            .await
            .unwrap()
            .is_empty()
    );
}

// ---- the commit ----------------------------------------------------------------------------------------------------

#[tokio::test]
async fn the_first_commit_inserts_the_summary_marks_the_range_and_enqueues_the_usage_event_together()
 {
    let env = WorkerEnv::new().await;
    let turns = four(&env).await;
    let target = boundary(turns[1].assistant_message, 21);

    let outcome = store(&env)
        .commit(&commit(&env, None, target))
        .await
        .unwrap();

    assert_eq!(outcome, CommitOutcome::Committed);
    let summary = env.summary(env.chat).await.unwrap();
    assert_eq!(summary.text, "the committed summary");
    assert_eq!(summary.frontier_id, turns[1].assistant_message);
    assert_eq!(summary.frontier_at, ts(21));
    assert_eq!(summary.token_estimate, 30);
    assert_eq!(
        summary.created_at,
        crate::attachment_test_env::AttEnv::stamp(env.now())
    );
    assert_eq!(summary.updated_at, summary.created_at);
    assert_eq!(
        env.compressed_ids(env.chat).await,
        vec![
            turns[0].user_message,
            turns[0].assistant_message,
            turns[1].user_message,
            turns[1].assistant_message
        ]
    );
    let events = env.usage().await;
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].billing_outcome, BillingOutcome::SystemTask);
    assert_eq!(events[0].settlement_method, SettlementMethod::None);
    assert_eq!(events[0].usage, Some(usage()));
    assert_eq!(events[0].effective_model, "sum");
    assert_eq!(events[0].timestamp, env.now());
    assert_eq!(
        env.quota.bucket_count().await,
        0,
        "no quota_usage row is touched"
    );
}

#[tokio::test]
async fn a_cas_commit_updates_the_row_in_place_and_marks_only_the_new_range() {
    let env = WorkerEnv::new().await;
    let turns = four(&env).await;
    env.seed_summary(env.chat, "old", turns[0].assistant_message, 11, 9)
        .await;
    let before = env.summary(env.chat).await.unwrap();
    env.advance(30);

    let outcome = store(&env)
        .commit(&commit(
            &env,
            Some(boundary(turns[0].assistant_message, 11)),
            boundary(turns[2].assistant_message, 31),
        ))
        .await
        .unwrap();

    assert_eq!(outcome, CommitOutcome::Committed);
    let after = env.summary(env.chat).await.unwrap();
    assert_eq!(after.text, "the committed summary");
    assert_eq!(after.frontier_id, turns[2].assistant_message);
    assert_eq!(after.created_at, before.created_at);
    assert_ne!(after.updated_at, before.updated_at);
    assert_eq!(env.count("thread_summaries").await, 1);
    assert_eq!(
        env.compressed_ids(env.chat).await,
        vec![
            turns[1].user_message,
            turns[1].assistant_message,
            turns[2].user_message,
            turns[2].assistant_message
        ],
        "turn 1 (at or before the base) and turn 4 (after the target) are untouched"
    );
}

#[tokio::test]
async fn a_soft_deleted_or_missing_target_commits_nothing_at_all() {
    let env = WorkerEnv::new().await;
    let turns = four(&env).await;
    env.m
        .quota
        .db
        .soft_delete("messages", turns[1].assistant_message)
        .await;
    let flags = env.compressed_ids(env.chat).await;

    let deleted = store(&env)
        .commit(&commit(
            &env,
            None,
            boundary(turns[1].assistant_message, 21),
        ))
        .await
        .unwrap();
    let missing = store(&env)
        .commit(&commit(&env, None, boundary(Uuid::from_u128(0xBAD), 99)))
        .await
        .unwrap();

    assert_eq!(
        (deleted, missing),
        (
            CommitOutcome::FrontierDeleted,
            CommitOutcome::FrontierDeleted
        )
    );
    assert!(env.summary(env.chat).await.is_none());
    assert_eq!(env.compressed_ids(env.chat).await, flags);
    assert!(env.usage().await.is_empty());
}

#[tokio::test]
async fn a_cas_commit_without_a_summary_row_is_base_missing() {
    let env = WorkerEnv::new().await;
    let turns = four(&env).await;

    let outcome = store(&env)
        .commit(&commit(
            &env,
            Some(boundary(turns[0].assistant_message, 11)),
            boundary(turns[2].assistant_message, 31),
        ))
        .await
        .unwrap();

    assert_eq!(outcome, CommitOutcome::BaseMissing);
    assert!(env.summary(env.chat).await.is_none(), "nothing is written");
    assert!(env.compressed_ids(env.chat).await.is_empty());
    assert!(env.usage().await.is_empty());
}

#[tokio::test]
async fn a_cas_commit_from_a_stale_base_is_a_conflict_and_changes_nothing() {
    let env = WorkerEnv::new().await;
    let turns = four(&env).await;
    env.seed_summary(env.chat, "newer", turns[2].assistant_message, 31, 9)
        .await;
    let before = env.summary(env.chat).await;

    let outcome = store(&env)
        .commit(&commit(
            &env,
            Some(boundary(turns[0].assistant_message, 11)),
            boundary(turns[1].assistant_message, 21),
        ))
        .await
        .unwrap();

    assert_eq!(outcome, CommitOutcome::CasConflict);
    assert_eq!(env.summary(env.chat).await, before);
    assert!(env.compressed_ids(env.chat).await.is_empty());
    assert!(env.usage().await.is_empty());
}

#[tokio::test]
async fn a_first_summary_over_an_existing_row_is_a_conflict_through_the_unique_chat_id() {
    let env = WorkerEnv::new().await;
    let turns = four(&env).await;
    env.seed_summary(env.chat, "already", turns[0].assistant_message, 11, 9)
        .await;

    let outcome = store(&env)
        .commit(&commit(
            &env,
            None,
            boundary(turns[1].assistant_message, 21),
        ))
        .await
        .unwrap();

    assert_eq!(outcome, CommitOutcome::CasConflict);
    assert_eq!(env.summary(env.chat).await.unwrap().text, "already");
    assert!(env.compressed_ids(env.chat).await.is_empty());
    assert!(env.usage().await.is_empty());
}

#[tokio::test]
async fn committing_the_same_range_twice_commits_it_once() {
    let env = WorkerEnv::new().await;
    let turns = four(&env).await;
    let task = commit(&env, None, boundary(turns[1].assistant_message, 21));

    let first = store(&env).commit(&task).await.unwrap();
    let second = store(&env).commit(&task).await.unwrap();

    assert_eq!(
        (first, second),
        (CommitOutcome::Committed, CommitOutcome::CasConflict)
    );
    assert_eq!(env.usage().await.len(), 1);
}

#[tokio::test]
async fn the_commit_never_marks_messages_of_another_chat() {
    let env = WorkerEnv::new().await;
    let turns = four(&env).await;
    let other = env.quota.seed_chat(2).await;
    let foreign = env.m.seed_turn_in(other, &SeedTurn::completed(7, 15)).await;

    store(&env)
        .commit(&commit(
            &env,
            None,
            boundary(turns[3].assistant_message, 41),
        ))
        .await
        .unwrap();

    assert!(
        env.compressed_ids(other).await.is_empty(),
        "the message of turn 7 (created between) is untouched"
    );
    let _ = foreign;
}

#[tokio::test]
async fn a_soft_deleted_message_inside_the_range_stays_unflagged() {
    let env = WorkerEnv::new().await;
    let turns = four(&env).await;
    env.m
        .quota
        .db
        .soft_delete("messages", turns[0].assistant_message)
        .await;

    store(&env)
        .commit(&commit(
            &env,
            None,
            boundary(turns[1].assistant_message, 21),
        ))
        .await
        .unwrap();

    let flagged = env.compressed_ids(env.chat).await;
    assert!(
        !flagged.contains(&turns[0].assistant_message),
        "{flagged:?}"
    );
    assert_eq!(flagged.len(), 3);
}

#[tokio::test]
async fn the_usage_event_is_enqueued_in_the_same_transaction_as_the_summary() {
    let env = WorkerEnv::new().await;
    let turns = four(&env).await;
    env.exec("CREATE TRIGGER fail_outbox BEFORE INSERT ON mini_chat__outbox_body BEGIN SELECT RAISE(ABORT, 'injected failure'); END")
        .await;

    let result = store(&env)
        .commit(&commit(
            &env,
            None,
            boundary(turns[1].assistant_message, 21),
        ))
        .await;

    assert!(result.is_err());
    assert!(
        env.summary(env.chat).await.is_none(),
        "the summary rolled back with the usage event"
    );
    assert!(env.compressed_ids(env.chat).await.is_empty());
}

#[tokio::test]
async fn a_commit_keeps_working_on_a_file_database_with_a_pool_of_connections() {
    let env = WorkerEnv::with(crate::worker_test_env::WorkerOptions {
        file_db: true,
        ..crate::worker_test_env::WorkerOptions::default()
    })
    .await;
    let turns = four(&env).await;

    let outcome = store(&env)
        .commit(&commit(
            &env,
            None,
            boundary(turns[3].assistant_message, 41),
        ))
        .await
        .unwrap();

    assert_eq!(outcome, CommitOutcome::Committed);
    assert_eq!(env.compressed_ids(env.chat).await.len(), 8);
}
