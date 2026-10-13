#![allow(clippy::unwrap_used, clippy::expect_used)]

//! Delivery of the usage and audit outbox messages to the plugins on the success path (DESIGN 5.6,
//! "Audit plugin and audit outbox"): every settled turn is published to the model-policy plugin
//! exactly once and delivered to the audit plugin exactly once, replays and rejected requests
//! publish nothing. Failure handling (retry, dead letters, a missing audit plugin) is in
//! `outbox_failures.rs`.

mod common;
mod live;

use std::collections::HashSet;

use common::{
    AUDIT_QUEUE, PROVIDER_RESPONSE_ID, USAGE_QUEUE, assert_quota_exhausted, complete_turn,
    enqueue_chat, env_user_chat, env_with_user_limits, expect_stream, failed_script, new_chat,
    send_text, send_with_id,
};
use mini_chat::test_support::{TestEnv, TestUser};
use serde_json::{Value, json};
use uuid::Uuid;

fn dedupe_keys(events: &[Value]) -> Vec<String> {
    events
        .iter()
        .map(|event| event["dedupe_key"].as_str().unwrap().to_owned())
        .collect()
}

/// One completed, one failed (no usage) and one cancelled turn of `user`.
async fn three_turns(env: &TestEnv, user: &TestUser, chat: &str) {
    complete_turn(env, user, chat, "one", "A").await;
    enqueue_chat(env, failed_script("boom", None));
    expect_stream(&send_text(env, user, chat, "two").await);
    let request_id = Uuid::new_v4();
    let stream = live::start_hung_turn(env, user, chat, request_id, "three", &["par"]).await;
    live::disconnect_and_wait_cancelled(env, chat, request_id, stream).await;
}

#[tokio::test]
async fn draining_publishes_every_settled_turn_exactly_once() {
    let (env, user, chat) = env_user_chat().await;
    three_turns(&env, &user, &chat).await;

    env.drain_outbox().await;
    env.drain_outbox().await;

    let published = env.published_usage();
    assert_eq!(published.len(), 3, "{published:?}");
    let keys = dedupe_keys(&published);
    assert_eq!(
        keys.iter().collect::<HashSet<_>>().len(),
        3,
        "no event is published twice: {keys:?}"
    );
    let enqueued = env.outbox_payloads(USAGE_QUEUE).await;
    assert_eq!(enqueued.len(), 3);
    assert_eq!(
        keys.iter().collect::<HashSet<_>>(),
        dedupe_keys(&enqueued).iter().collect::<HashSet<_>>()
    );
}

#[tokio::test]
async fn the_plugin_receives_the_enqueued_usage_event_verbatim() {
    let (env, user, chat) = env_user_chat().await;
    three_turns(&env, &user, &chat).await;
    env.drain_outbox().await;

    let published = env.published_usage();

    for event in env.outbox_payloads(USAGE_QUEUE).await {
        let key = event["dedupe_key"].clone();
        let received = published
            .iter()
            .find(|candidate| candidate["dedupe_key"] == key)
            .unwrap_or_else(|| panic!("{key} was not published: {published:?}"));
        assert_eq!(received, &event);
    }
}

#[tokio::test]
async fn every_billing_outcome_is_published() {
    let (env, user, chat) = env_user_chat().await;
    three_turns(&env, &user, &chat).await;
    env.drain_outbox().await;

    let mut outcomes: Vec<(String, String)> = env
        .published_usage()
        .iter()
        .map(|event| {
            (
                event["billing_outcome"].as_str().unwrap().to_owned(),
                event["settlement_method"].as_str().unwrap().to_owned(),
            )
        })
        .collect();
    outcomes.sort();

    assert_eq!(
        outcomes,
        [
            ("aborted".to_owned(), "estimated".to_owned()),
            ("completed".to_owned(), "actual".to_owned()),
            ("failed".to_owned(), "estimated".to_owned()),
        ]
    );
}

#[tokio::test]
async fn the_audit_plugin_receives_every_audit_event_exactly_once() {
    let (env, user, chat) = env_user_chat().await;
    three_turns(&env, &user, &chat).await;

    env.drain_outbox().await;
    env.drain_outbox().await;

    let delivered = env.delivered_audit();
    assert_eq!(delivered.len(), 3, "{delivered:?}");
    let enqueued = env.outbox_payloads(AUDIT_QUEUE).await;
    for event in &enqueued {
        assert!(
            delivered.contains(event),
            "{event} was not delivered: {delivered:?}"
        );
    }
    let mut types: Vec<&str> = delivered
        .iter()
        .map(|event| event["event_type"].as_str().unwrap())
        .collect();
    types.sort_unstable();
    assert_eq!(types, ["turn_completed", "turn_failed", "turn_failed"]);
}

#[tokio::test]
async fn a_replayed_request_publishes_and_delivers_nothing_more() {
    let (env, user, chat) = env_user_chat().await;
    let request_id = Uuid::new_v4();
    expect_stream(&send_with_id(&env, &user, &chat, "hello", request_id).await);
    env.drain_outbox().await;

    let replay = expect_stream(&send_with_id(&env, &user, &chat, "hello", request_id).await);
    env.drain_outbox().await;

    assert_eq!(common::terminal(&replay).event, "done");
    assert_eq!(env.outbox_payloads(USAGE_QUEUE).await.len(), 1);
    assert_eq!(env.outbox_payloads(AUDIT_QUEUE).await.len(), 1);
    assert_eq!(env.published_usage().len(), 1);
    assert_eq!(env.delivered_audit().len(), 1);
}

#[tokio::test]
async fn rejected_requests_publish_nothing() {
    let env = env_with_user_limits((1000, 1000), (1000, 1000)).await;
    let user = TestUser::new();
    let chat = new_chat(&env, &user).await;

    let quota = send_text(&env, &user, &chat, "hello").await;
    let invalid = send_text(&env, &user, &chat, "   ").await;
    env.drain_outbox().await;

    assert_quota_exhausted(&quota, "tokens");
    assert_eq!(invalid.status, http::StatusCode::BAD_REQUEST);
    assert!(env.published_usage().is_empty());
    assert!(env.delivered_audit().is_empty());
}

#[tokio::test]
async fn published_events_belong_to_their_own_tenant_and_user() {
    let env = common::env_with_policy(common::policy(common::two_model_catalog())).await;
    let alice = TestUser::new();
    let bob = TestUser::new();
    let alice_chat = new_chat(&env, &alice).await;
    let bob_chat = new_chat(&env, &bob).await;
    complete_turn(&env, &alice, &alice_chat, "hello", "A").await;
    complete_turn(&env, &bob, &bob_chat, "hello", "B").await;
    env.drain_outbox().await;

    let published = env.published_usage();

    assert_eq!(published.len(), 2);
    for (user, chat) in [(&alice, &alice_chat), (&bob, &bob_chat)] {
        let event = published
            .iter()
            .find(|event| event["chat_id"] == json!(chat))
            .unwrap_or_else(|| panic!("no event for {chat}: {published:?}"));
        assert_eq!(event["tenant_id"], json!(user.tenant_id.to_string()));
        assert_eq!(event["user_id"], json!(user.user_id.to_string()));
        assert!(
            event["dedupe_key"]
                .as_str()
                .unwrap()
                .starts_with(&user.tenant_id.simple().to_string()),
            "the dedupe key starts with the tenant: {event}"
        );
    }
}

#[tokio::test]
async fn delivered_events_expose_no_provider_identifier() {
    let (env, user, chat) = env_user_chat().await;
    three_turns(&env, &user, &chat).await;
    env.drain_outbox().await;

    let everything =
        serde_json::to_string(&[env.published_usage(), env.delivered_audit()]).unwrap();

    assert!(!everything.contains(PROVIDER_RESPONSE_ID), "{everything}");
    assert!(!everything.contains("resp_"), "{everything}");
}
