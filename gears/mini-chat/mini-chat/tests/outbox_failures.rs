#![allow(clippy::unwrap_used, clippy::expect_used)]

//! Failure handling of the usage and audit outbox handlers (DESIGN 5.6 "Shared Outbox Processing
//! Model", "Audit plugin and audit outbox", B.9.3, ADR-0009): a transient plugin failure is retried
//! and the event is published exactly once later, a permanent plugin failure and a payload that cannot
//! be deserialized are rejected into the dead-letter store, an audit event is acknowledged and dropped
//! when no audit plugin is registered, and a rejected message never blocks the ones behind it.
//!
//! The handler outcomes (`"ok"`, `"retry"`, `"reject"`) per queue come from
//! `TestEnv::outbox_outcomes`; dead letters are read from the shared outbox tables.

mod billing;
mod common;
mod live;

use billing::{dead_letters, wait_for_outcomes};
use common::{
    AUDIT_QUEUE, USAGE_QUEUE, complete_turn, dedupe_key, env_user_chat, expect_quota_row,
    expect_stream, only_turn, quota_rows, send_text,
};
use mini_chat::test_support::{TestEnv, TestEnvBuilder, TestUser};
use serde_json::{Value, json};
use uuid::Uuid;

fn outcomes(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| (*value).to_owned()).collect()
}

// ===========================================================================
// A transient publication failure is retried and published once
// ===========================================================================

#[tokio::test]
async fn a_transient_publish_failure_is_retried_and_the_event_is_published_once() {
    let (env, user, chat) = env_user_chat().await;
    env.fail_next_usage_publishes(1);
    let turn = complete_turn(&env, &user, &chat, "hello", "answer").await;

    env.drain_outbox().await;

    let published = env.published_usage();
    assert_eq!(published.len(), 1, "{published:?}");
    let row = only_turn(&env, &chat).await;
    assert_eq!(
        published[0]["dedupe_key"],
        json!(dedupe_key(user.tenant_id, row.id, turn.request_id))
    );
    assert_eq!(env.usage_publish_attempts(), 2, "one failure, one success");
    assert_eq!(
        wait_for_outcomes(&env, USAGE_QUEUE, 2).await,
        outcomes(&["retry", "ok"])
    );
    assert!(dead_letters(&env, USAGE_QUEUE).await.is_empty());
}

#[tokio::test]
async fn several_transient_failures_in_a_row_still_end_in_one_publication() {
    let (env, user, chat) = env_user_chat().await;
    env.fail_next_usage_publishes(3);
    complete_turn(&env, &user, &chat, "hello", "answer").await;

    env.drain_outbox().await;

    assert_eq!(env.published_usage().len(), 1);
    assert_eq!(env.usage_publish_attempts(), 4);
    assert_eq!(
        wait_for_outcomes(&env, USAGE_QUEUE, 4).await,
        outcomes(&["retry", "retry", "retry", "ok"])
    );
}

#[tokio::test]
async fn a_retried_event_is_enqueued_once_and_the_settlement_is_unaffected() {
    let (env, user, chat) = env_user_chat().await;
    env.fail_next_usage_publishes(2);
    complete_turn(&env, &user, &chat, "hello", "answer").await;
    let before = quota_rows(&env, &user).await;

    env.drain_outbox().await;

    assert_eq!(
        env.outbox_payloads(USAGE_QUEUE).await.len(),
        1,
        "handler retries do not enqueue again"
    );
    assert_eq!(env.outbox_payloads(AUDIT_QUEUE).await.len(), 1);
    assert_eq!(
        quota_rows(&env, &user).await,
        before,
        "publishing never touches the quota"
    );
    assert_eq!(expect_quota_row(&before, "daily", "total").calls, 1);
}

#[tokio::test]
async fn a_failing_publication_does_not_lose_or_duplicate_the_events_behind_it() {
    let (env, user, chat) = env_user_chat().await;
    env.fail_next_usage_publishes(1);
    complete_turn(&env, &user, &chat, "one", "A").await;
    complete_turn(&env, &user, &chat, "two", "B").await;
    complete_turn(&env, &user, &chat, "three", "C").await;

    env.drain_outbox().await;

    let published = env.published_usage();
    assert_eq!(published.len(), 3, "{published:?}");
    let mut keys: Vec<&str> = published
        .iter()
        .map(|event| event["dedupe_key"].as_str().unwrap())
        .collect();
    keys.sort_unstable();
    keys.dedup();
    assert_eq!(keys.len(), 3, "each event exactly once");
}

#[tokio::test]
async fn the_audit_event_is_delivered_once_even_while_the_usage_event_is_retried() {
    let (env, user, chat) = env_user_chat().await;
    env.fail_next_usage_publishes(1);
    complete_turn(&env, &user, &chat, "hello", "answer").await;

    env.drain_outbox().await;

    assert_eq!(env.delivered_audit().len(), 1);
    assert_eq!(env.published_usage().len(), 1);
}

// ===========================================================================
// A permanent publication failure is dead-lettered
// ===========================================================================

#[tokio::test]
async fn a_permanent_publish_failure_is_rejected_into_the_dead_letter_store() {
    let (env, user, chat) = env_user_chat().await;
    env.reject_next_usage_publishes(1);
    complete_turn(&env, &user, &chat, "hello", "answer").await;

    assert_eq!(
        wait_for_outcomes(&env, USAGE_QUEUE, 1).await,
        outcomes(&["reject"])
    );

    assert!(env.published_usage().is_empty());
    assert_eq!(
        env.usage_publish_attempts(),
        1,
        "a permanent failure is not retried"
    );
    let dead = dead_letters(&env, USAGE_QUEUE).await;
    assert_eq!(dead.len(), 1, "{dead:?}");
    assert_eq!(dead[0].status, "pending");
    let payload: Value = serde_json::from_slice(&dead[0].payload).unwrap();
    assert_eq!(payload, env.outbox_payloads(USAGE_QUEUE).await[0]);
    assert!(dead[0].last_error.is_some(), "the reject reason is kept");
}

#[tokio::test]
async fn a_rejected_event_does_not_block_the_next_one() {
    let (env, user, chat) = env_user_chat().await;
    env.reject_next_usage_publishes(1);
    complete_turn(&env, &user, &chat, "one", "A").await;
    complete_turn(&env, &user, &chat, "two", "B").await;

    assert_eq!(
        wait_for_outcomes(&env, USAGE_QUEUE, 2).await,
        outcomes(&["reject", "ok"])
    );

    assert_eq!(env.published_usage().len(), 1);
    assert_eq!(dead_letters(&env, USAGE_QUEUE).await.len(), 1);
}

// ===========================================================================
// Malformed payloads
// ===========================================================================

fn user_partition(user: &TestUser) -> Uuid {
    user.tenant_id
}

#[tokio::test]
async fn a_malformed_usage_payload_is_rejected_without_calling_the_plugin() {
    let (env, user, _chat) = env_user_chat().await;

    env.enqueue_raw_outbox(USAGE_QUEUE, user_partition(&user), b"this is not json")
        .await;

    assert_eq!(
        wait_for_outcomes(&env, USAGE_QUEUE, 1).await,
        outcomes(&["reject"])
    );
    assert_eq!(
        env.usage_publish_attempts(),
        0,
        "the payload is checked before the plugin"
    );
    assert!(env.published_usage().is_empty());
    let dead = dead_letters(&env, USAGE_QUEUE).await;
    assert_eq!(dead.len(), 1, "{dead:?}");
    assert_eq!(dead[0].payload, b"this is not json");
    assert!(dead[0].last_error.is_some());
}

#[tokio::test]
async fn a_json_payload_of_the_wrong_shape_is_rejected_too() {
    let (env, user, _chat) = env_user_chat().await;

    env.enqueue_raw_outbox(
        USAGE_QUEUE,
        user_partition(&user),
        json!({ "tenant_id": "not-a-uuid", "usage": 5 })
            .to_string()
            .as_bytes(),
    )
    .await;

    assert_eq!(
        wait_for_outcomes(&env, USAGE_QUEUE, 1).await,
        outcomes(&["reject"])
    );
    assert_eq!(dead_letters(&env, USAGE_QUEUE).await.len(), 1);
    assert!(env.published_usage().is_empty());
}

#[tokio::test]
async fn a_malformed_message_does_not_block_a_valid_event_behind_it() {
    let (env, user, chat) = env_user_chat().await;
    env.enqueue_raw_outbox(USAGE_QUEUE, user_partition(&user), b"garbage")
        .await;
    complete_turn(&env, &user, &chat, "hello", "answer").await;

    assert_eq!(
        wait_for_outcomes(&env, USAGE_QUEUE, 2).await,
        outcomes(&["reject", "ok"])
    );

    assert_eq!(env.published_usage().len(), 1);
    assert_eq!(dead_letters(&env, USAGE_QUEUE).await.len(), 1);
}

#[tokio::test]
async fn a_malformed_audit_payload_is_rejected_and_nothing_is_delivered() {
    let (env, user, _chat) = env_user_chat().await;

    env.enqueue_raw_outbox(
        AUDIT_QUEUE,
        user_partition(&user),
        b"\x00\x01 not an audit event",
    )
    .await;

    assert_eq!(
        wait_for_outcomes(&env, AUDIT_QUEUE, 1).await,
        outcomes(&["reject"])
    );
    assert!(env.delivered_audit().is_empty());
    assert_eq!(dead_letters(&env, AUDIT_QUEUE).await.len(), 1);
}

// ===========================================================================
// No audit plugin registered: acknowledged and dropped
// ===========================================================================

async fn env_without_audit_plugin() -> (TestEnv, TestUser, String) {
    let env = TestEnvBuilder::new().without_audit_plugin().build().await;
    let user = TestUser::new();
    let chat = common::new_chat(&env, &user).await;
    (env, user, chat)
}

#[tokio::test]
async fn without_an_audit_plugin_the_audit_event_is_acknowledged_and_dropped() {
    let (env, user, chat) = env_without_audit_plugin().await;

    complete_turn(&env, &user, &chat, "hello", "answer").await;

    assert_eq!(
        wait_for_outcomes(&env, AUDIT_QUEUE, 1).await,
        outcomes(&["ok"])
    );
    assert!(env.delivered_audit().is_empty());
    assert!(
        dead_letters(&env, AUDIT_QUEUE).await.is_empty(),
        "a dropped event is not dead-lettered"
    );
    assert_eq!(
        env.outbox_payloads(AUDIT_QUEUE).await.len(),
        1,
        "the event was enqueued; only its delivery was dropped"
    );
}

#[tokio::test]
async fn every_audit_event_is_dropped_without_a_plugin_as_the_lookup_is_repeated() {
    let (env, user, chat) = env_without_audit_plugin().await;

    complete_turn(&env, &user, &chat, "one", "A").await;
    complete_turn(&env, &user, &chat, "two", "B").await;

    assert_eq!(
        wait_for_outcomes(&env, AUDIT_QUEUE, 2).await,
        outcomes(&["ok", "ok"])
    );
    assert!(env.delivered_audit().is_empty());
}

#[tokio::test]
async fn the_usage_event_is_published_even_without_an_audit_plugin() {
    let (env, user, chat) = env_without_audit_plugin().await;

    complete_turn(&env, &user, &chat, "hello", "answer").await;

    assert_eq!(
        wait_for_outcomes(&env, USAGE_QUEUE, 1).await,
        outcomes(&["ok"])
    );
    assert_eq!(env.published_usage().len(), 1);
}

#[tokio::test]
async fn a_malformed_audit_payload_is_rejected_even_when_no_plugin_is_registered() {
    let (env, user, _chat) = env_without_audit_plugin().await;

    env.enqueue_raw_outbox(AUDIT_QUEUE, user_partition(&user), b"garbage")
        .await;

    assert_eq!(
        wait_for_outcomes(&env, AUDIT_QUEUE, 1).await,
        outcomes(&["reject"]),
        "the payload is checked before the plugin is looked up"
    );
    assert_eq!(dead_letters(&env, AUDIT_QUEUE).await.len(), 1);
}

#[tokio::test]
async fn turns_keep_completing_while_the_audit_events_are_dropped() {
    let (env, user, chat) = env_without_audit_plugin().await;

    let events = expect_stream(&send_text(&env, &user, &chat, "hello").await);

    assert_eq!(common::terminal(&events).event, "done");
}
