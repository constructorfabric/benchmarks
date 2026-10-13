#![allow(clippy::unwrap_used, clippy::expect_used)]

//! The provider task is supervised: a panic of the provider interaction and a transient finalize
//! failure still end with a terminal turn, a released reserve and exactly one usage event
//! (DESIGN sections 3.3, 3.6 "Streaming Cancellation", 5.7).

mod common;
mod live;

use common::{
    USAGE_QUEUE, enqueue_chat, env_user_chat, expect_stream, failed_script, partial_frames,
    send_text, stream_request_id, terminal, total_reserved, turn_row,
};
use mini_chat::test_support::{ScriptedResponse, TestEnv};

async fn only_usage_event(env: &TestEnv) {
    env.drain_outbox().await;
    assert_eq!(
        env.outbox_payloads(USAGE_QUEUE).await.len(),
        1,
        "exactly one usage event is enqueued"
    );
    assert_eq!(env.published_usage().len(), 1);
}

#[tokio::test]
async fn a_provider_panic_mid_stream_finalizes_the_turn_failed_and_releases_the_reserve() {
    let (env, user, chat) = env_user_chat().await;
    enqueue_chat(
        &env,
        ScriptedResponse::SseThenPanic(partial_frames(&["par"])),
    );

    let events = expect_stream(&send_text(&env, &user, &chat, "hello").await);

    let end = terminal(&events);
    assert_eq!(end.event, "error", "{events:?}");
    assert_eq!(end.data["code"], serde_json::json!("provider_error"));
    let turn = turn_row(&env, &chat, stream_request_id(&events))
        .await
        .unwrap();
    assert_eq!(turn.state, "failed");
    assert_eq!(turn.error_code.as_deref(), Some("provider_error"));
    assert!(turn.has_completed_at);
    assert_eq!(total_reserved(&env).await, 0, "the reserve is released");
    only_usage_event(&env).await;
}

#[tokio::test]
async fn the_chat_accepts_a_new_turn_after_a_provider_panic() {
    let (env, user, chat) = env_user_chat().await;
    enqueue_chat(&env, ScriptedResponse::SseThenPanic(partial_frames(&[])));
    expect_stream(&send_text(&env, &user, &chat, "first").await);

    common::enqueue_text(&env, &["again"], 10, 5);
    let events = common::send_ok(&env, &user, &chat, "second").await;

    assert_eq!(terminal(&events).event, "done");
}

#[tokio::test]
async fn a_transient_finalize_failure_of_a_failed_turn_is_retried() {
    let (env, user, chat) = env_user_chat().await;
    enqueue_chat(&env, failed_script("boom", None));
    env.fail_next_finalizations(1);

    let events = expect_stream(&send_text(&env, &user, &chat, "hello").await);

    let end = terminal(&events);
    assert_eq!(end.data["code"], serde_json::json!("provider_error"));
    let turn = turn_row(&env, &chat, stream_request_id(&events))
        .await
        .unwrap();
    assert_eq!(turn.state, "failed", "the retry finalized the turn");
    assert!(turn.has_completed_at);
    assert_eq!(total_reserved(&env).await, 0);
    only_usage_event(&env).await;
}

#[tokio::test]
async fn a_finalize_that_keeps_failing_is_given_up_after_three_attempts() {
    let (env, user, chat) = env_user_chat().await;
    enqueue_chat(&env, failed_script("boom", None));
    env.fail_next_finalizations(3);

    let events = expect_stream(&send_text(&env, &user, &chat, "hello").await);

    // The original error is still reported; the turn stays `running` for the orphan watchdog.
    assert_eq!(
        terminal(&events).data["code"],
        serde_json::json!("provider_error")
    );
    let turn = turn_row(&env, &chat, stream_request_id(&events))
        .await
        .unwrap();
    assert_eq!(turn.state, "running");
}

#[tokio::test]
async fn a_transient_finalize_failure_of_a_cancelled_turn_is_retried() {
    let (env, user, chat) = env_user_chat().await;
    env.fail_next_finalizations(1);
    let request_id = uuid::Uuid::new_v4();
    let stream = live::start_hung_turn(&env, &user, &chat, request_id, "hello", &["par"]).await;

    stream.disconnect();

    env.wait_for_turn_state(chat_uuid(&chat), request_id, "cancelled")
        .await;
    assert_eq!(total_reserved(&env).await, 0);
}

fn chat_uuid(chat: &str) -> uuid::Uuid {
    uuid::Uuid::parse_str(chat).unwrap()
}
