#![allow(clippy::unwrap_used, clippy::expect_used)]

//! Idempotency, replay and the parallel-turn guard of `messages:stream` (DESIGN section 3.3
//! "Idempotency", 3.9 "Idempotency Rules" and "Parallel Turn Policy (P1)", ADR-0010).

mod common;
mod live;

use common::{
    AUDIT_QUEUE, USAGE_QUEUE, assert_aborted, complete_turn, content_type, enqueue_chat,
    enqueue_text, env_two_models, env_user_chat, env_with_policy, exec_sql, expect_stream,
    failed_script, merge, message_row_count, named, names, new_chat, only_turn, policy, quota_rows,
    send_with_id, started, stream_message_id, stream_request_id, streamed_text, terminal,
    tiny_limits, total_reserved, turn_row, turn_row_count, turn_rows, two_model_catalog, uuid_hex,
};
use mini_chat::test_support::{ScriptedResponse, TestUser};
use serde_json::json;
use uuid::Uuid;

// ===========================================================================
// Replay of a completed turn
// ===========================================================================

#[tokio::test]
async fn replay_of_a_completed_turn_streams_the_stored_text_with_is_new_turn_false() {
    let (env, user, chat) = env_user_chat().await;
    let request_id = Uuid::new_v4();
    enqueue_text(&env, &["Hel", "lo ", "world"], 10, 5);
    let original = expect_stream(&send_with_id(&env, &user, &chat, "hello", request_id).await);

    let replay = expect_stream(&send_with_id(&env, &user, &chat, "hello", request_id).await);

    assert_eq!(
        names(&replay),
        ["stream_started", "delta", "done"],
        "{replay:?}"
    );
    let start = started(&replay);
    assert_eq!(start.data["is_new_turn"], json!(false));
    assert_eq!(start.data["request_id"], json!(request_id.to_string()));
    assert_eq!(
        start.data["message_id"].as_str().unwrap(),
        stream_message_id(&original),
        "replay carries the persisted assistant message id"
    );
    let deltas = named(&replay, "delta");
    assert_eq!(deltas.len(), 1, "one delta with the full persisted text");
    assert_eq!(
        deltas[0].data,
        json!({ "type": "text", "content": "Hello world" })
    );
    assert_eq!(streamed_text(&replay), streamed_text(&original));
}

#[tokio::test]
async fn replay_is_an_event_stream_with_status_200() {
    let (env, user, chat) = env_user_chat().await;
    let request_id = Uuid::new_v4();
    send_with_id(&env, &user, &chat, "hello", request_id).await;

    let resp = send_with_id(&env, &user, &chat, "hello", request_id).await;

    assert_eq!(resp.status, http::StatusCode::OK);
    assert!(content_type(&resp).starts_with("text/event-stream"));
}

#[tokio::test]
async fn replay_does_not_call_the_provider() {
    let (env, user, chat) = env_user_chat().await;
    let request_id = Uuid::new_v4();
    send_with_id(&env, &user, &chat, "hello", request_id).await;
    assert_eq!(env.provider().chat_requests().len(), 1);

    send_with_id(&env, &user, &chat, "hello", request_id).await;
    send_with_id(&env, &user, &chat, "hello", request_id).await;

    assert_eq!(
        env.provider().chat_requests().len(),
        1,
        "replays must not issue a provider request"
    );
}

#[tokio::test]
async fn replay_does_not_touch_quota_usage() {
    let (env, user, chat) = env_user_chat().await;
    let request_id = Uuid::new_v4();
    send_with_id(&env, &user, &chat, "hello", request_id).await;
    let before = quota_rows(&env, &user).await;
    assert!(!before.is_empty(), "the original turn settled quota");

    send_with_id(&env, &user, &chat, "hello", request_id).await;

    assert_eq!(
        quota_rows(&env, &user).await,
        before,
        "replay is side-effect free"
    );
    assert_eq!(total_reserved(&env).await, 0, "replay takes no reserve");
}

#[tokio::test]
async fn replay_enqueues_no_outbox_message() {
    let (env, user, chat) = env_user_chat().await;
    let request_id = Uuid::new_v4();
    send_with_id(&env, &user, &chat, "hello", request_id).await;
    let usage_before = env.outbox_payloads(USAGE_QUEUE).await.len();
    let audit_before = env.outbox_payloads(AUDIT_QUEUE).await.len();
    assert_eq!(usage_before, 1);
    assert_eq!(audit_before, 1);

    send_with_id(&env, &user, &chat, "hello", request_id).await;

    assert_eq!(env.outbox_payloads(USAGE_QUEUE).await.len(), usage_before);
    assert_eq!(env.outbox_payloads(AUDIT_QUEUE).await.len(), audit_before);
}

#[tokio::test]
async fn replay_adds_no_messages_and_no_turns() {
    let (env, user, chat) = env_user_chat().await;
    let request_id = Uuid::new_v4();
    send_with_id(&env, &user, &chat, "hello", request_id).await;

    send_with_id(&env, &user, &chat, "hello", request_id).await;

    assert_eq!(message_row_count(&env).await, 2);
    assert_eq!(turn_row_count(&env).await, 1);
}

#[tokio::test]
async fn replay_ignores_the_content_of_the_repeated_request() {
    let (env, user, chat) = env_user_chat().await;
    let request_id = Uuid::new_v4();
    enqueue_text(&env, &["stored answer"], 10, 5);
    send_with_id(&env, &user, &chat, "original question", request_id).await;

    let replay =
        expect_stream(&send_with_id(&env, &user, &chat, "a different question", request_id).await);

    assert_eq!(streamed_text(&replay), "stored answer");
    assert_eq!(env.provider().chat_requests().len(), 1);
    assert_eq!(
        message_row_count(&env).await,
        2,
        "the new content is not stored"
    );
}

#[tokio::test]
async fn replay_done_carries_the_stored_usage_and_models_without_quota_warnings() {
    let (env, user, chat) = env_user_chat().await;
    let request_id = Uuid::new_v4();
    enqueue_text(&env, &["answer"], 321, 123);
    let original = expect_stream(&send_with_id(&env, &user, &chat, "hello", request_id).await);

    let replay = expect_stream(&send_with_id(&env, &user, &chat, "hello", request_id).await);

    let done = terminal(&replay);
    assert_eq!(done.event, "done");
    assert_eq!(
        done.data["usage"],
        json!({ "input_tokens": 321, "output_tokens": 123 })
    );
    let original_done = terminal(&original);
    for key in ["effective_model", "selected_model", "quota_decision"] {
        assert_eq!(
            done.data[key], original_done.data[key],
            "`{key}` differs on replay"
        );
    }
    assert!(
        done.data.get("quota_warnings").is_none(),
        "quota_warnings are absent on replay: {done:?}"
    );
}

#[tokio::test]
async fn replay_sends_no_citations() {
    let (env, user, chat) = env_user_chat().await;
    let request_id = Uuid::new_v4();
    let annotation = json!({
        "type": "url_citation", "start_index": 0, "end_index": 4,
        "url": "https://example.com/rust", "title": "Rust Language"
    });
    let mut frames = common::opening_frames();
    frames.extend(common::delta_frames(&["Rust rocks"]));
    frames.push(common::frame(
        "response.output_text.annotation.added",
        json!({ "item_id": "msg_0123456789abcdef", "output_index": 0, "content_index": 0,
                "annotation_index": 0, "annotation": annotation }),
    ));
    frames.extend(common::closing_frames(
        "Rust rocks",
        &json!([annotation]),
        Some((10, 5)),
        "response.completed",
    ));
    enqueue_chat(&env, ScriptedResponse::Sse(frames));
    let original =
        expect_stream(&send_with_id_ws(&env, &user, &chat, "what is rust", request_id).await);
    assert_eq!(named(&original, "citations").len(), 1, "{original:?}");

    let replay = expect_stream(&send_with_id(&env, &user, &chat, "what is rust", request_id).await);

    assert!(
        named(&replay, "citations").is_empty(),
        "citations are not persisted: {replay:?}"
    );
}

async fn send_with_id_ws(
    env: &mini_chat::test_support::TestEnv,
    user: &TestUser,
    chat: &str,
    content: &str,
    request_id: Uuid,
) -> mini_chat::test_support::TestResponse {
    common::send(
        env,
        user,
        chat,
        json!({
            "content": content,
            "request_id": request_id.to_string(),
            "web_search": { "enabled": true }
        }),
    )
    .await
}

#[tokio::test]
async fn replay_after_a_downgrade_rebuilds_the_downgrade_without_a_reason() {
    let env = env_with_policy(merge(
        policy(two_model_catalog()),
        json!({ "default_premium_limits": tiny_limits() }),
    ))
    .await;
    let user = TestUser::new();
    let chat = new_chat(&env, &user).await;
    let request_id = Uuid::new_v4();
    let original = expect_stream(&send_with_id(&env, &user, &chat, "hello", request_id).await);
    assert_eq!(
        terminal(&original).data["quota_decision"],
        json!("downgrade")
    );

    let replay = expect_stream(&send_with_id(&env, &user, &chat, "hello", request_id).await);

    let done = terminal(&replay);
    assert_eq!(done.data["quota_decision"], json!("downgrade"), "{done:?}");
    assert_eq!(done.data["effective_model"], json!(common::STANDARD_MODEL));
    assert_eq!(done.data["selected_model"], json!(common::PREMIUM_MODEL));
    assert_eq!(done.data["downgrade_from"], json!(common::PREMIUM_MODEL));
    assert!(
        done.data.get("downgrade_reason").is_none(),
        "the reason is not persisted, so replay omits it: {done:?}"
    );
}

#[tokio::test]
async fn replay_works_for_a_server_generated_request_id() {
    let (env, user, chat) = env_user_chat().await;
    let turn = complete_turn(&env, &user, &chat, "hello", "answer").await;

    let replay = expect_stream(&send_with_id(&env, &user, &chat, "hello", turn.request_id).await);

    assert_eq!(started(&replay).data["is_new_turn"], json!(false));
    assert_eq!(streamed_text(&replay), "answer");
}

#[tokio::test]
async fn a_replayed_turn_stays_completed_and_keeps_its_assistant_message() {
    let (env, user, chat) = env_user_chat().await;
    let turn = complete_turn(&env, &user, &chat, "hello", "answer").await;
    let before = only_turn(&env, &chat).await;

    send_with_id(&env, &user, &chat, "hello", turn.request_id).await;

    let after = only_turn(&env, &chat).await;
    assert_eq!(after.state, "completed");
    assert_eq!(after.assistant_message_id, before.assistant_message_id);
}

// ===========================================================================
// request_id conflicts
// ===========================================================================

#[tokio::test]
async fn reusing_the_request_id_of_a_failed_turn_is_409_request_id_conflict() {
    let (env, user, chat) = env_user_chat().await;
    let request_id = Uuid::new_v4();
    enqueue_chat(&env, failed_script("boom", None));
    send_with_id(&env, &user, &chat, "hello", request_id).await;
    let provider_calls = env.provider().chat_requests().len();

    let resp = send_with_id(&env, &user, &chat, "hello", request_id).await;

    assert_aborted(&resp, "request_id_conflict");
    assert_eq!(env.provider().chat_requests().len(), provider_calls);
    assert_eq!(
        turn_row_count(&env).await,
        1,
        "the key stays taken, no new turn"
    );
}

#[tokio::test]
async fn reusing_the_request_id_of_a_cancelled_turn_is_409_request_id_conflict() {
    let (env, user, chat) = env_user_chat().await;
    let request_id = Uuid::new_v4();
    let stream = live::start_hung_turn(&env, &user, &chat, request_id, "hello", &["par"]).await;
    live::disconnect_and_wait_cancelled(&env, &chat, request_id, stream).await;
    let provider_calls = env.provider().chat_requests().len();

    let resp = send_with_id(&env, &user, &chat, "hello", request_id).await;

    assert_aborted(&resp, "request_id_conflict");
    assert_eq!(env.provider().chat_requests().len(), provider_calls);
}

#[tokio::test]
async fn reusing_the_request_id_of_a_running_turn_is_409_request_id_conflict() {
    let (env, user, chat) = env_user_chat().await;
    let request_id = Uuid::new_v4();
    let stream = live::start_hung_turn(&env, &user, &chat, request_id, "hello", &["par"]).await;

    let resp = send_with_id(&env, &user, &chat, "hello", request_id).await;

    assert_aborted(&resp, "request_id_conflict");
    assert_eq!(
        env.provider().chat_requests().len(),
        1,
        "no second provider request"
    );
    stream.disconnect();
}

#[tokio::test]
async fn reusing_the_request_id_of_a_soft_deleted_turn_is_409_request_id_conflict() {
    let (env, user, chat) = env_user_chat().await;
    let request_id = Uuid::new_v4();
    send_with_id(&env, &user, &chat, "hello", request_id).await;
    exec_sql(
        &env,
        &format!(
            "UPDATE chat_turns SET deleted_at = '2000-01-01T00:00:00Z' WHERE hex(request_id) = '{}'",
            uuid_hex(request_id)
        ),
    )
    .await;

    let resp = send_with_id(&env, &user, &chat, "hello", request_id).await;

    assert_aborted(&resp, "request_id_conflict");
    assert_eq!(env.provider().chat_requests().len(), 1);
}

#[tokio::test]
async fn a_request_id_conflict_is_a_json_problem_without_an_event_stream() {
    let (env, user, chat) = env_user_chat().await;
    let request_id = Uuid::new_v4();
    enqueue_chat(&env, failed_script("boom", None));
    send_with_id(&env, &user, &chat, "hello", request_id).await;

    let resp = send_with_id(&env, &user, &chat, "hello", request_id).await;

    assert!(
        content_type(&resp).starts_with("application/problem+json"),
        "{}",
        content_type(&resp)
    );
    assert!(!content_type(&resp).contains("event-stream"));
}

#[tokio::test]
async fn the_same_request_id_may_be_used_in_a_different_chat() {
    let (env, user, chat) = env_user_chat().await;
    let other_chat = new_chat(&env, &user).await;
    let request_id = Uuid::new_v4();
    send_with_id(&env, &user, &chat, "hello", request_id).await;

    let events = expect_stream(&send_with_id(&env, &user, &other_chat, "hello", request_id).await);

    assert_eq!(started(&events).data["is_new_turn"], json!(true));
    assert_eq!(env.provider().chat_requests().len(), 2);
}

#[tokio::test]
async fn a_conflict_does_not_disturb_the_original_turn() {
    let (env, user, chat) = env_user_chat().await;
    let request_id = Uuid::new_v4();
    enqueue_chat(&env, failed_script("boom", None));
    send_with_id(&env, &user, &chat, "hello", request_id).await;
    let before = only_turn(&env, &chat).await;

    send_with_id(&env, &user, &chat, "hello", request_id).await;

    let after = only_turn(&env, &chat).await;
    assert_eq!(after.state, before.state);
    assert_eq!(after.error_code, before.error_code);
}

// ===========================================================================
// Parallel turn guard
// ===========================================================================

#[tokio::test]
async fn a_second_send_while_a_turn_is_running_is_409_turn_already_running() {
    let (env, user, chat) = env_user_chat().await;
    let running = Uuid::new_v4();
    let stream = live::start_hung_turn(&env, &user, &chat, running, "first", &["par"]).await;

    let resp = send_with_id(&env, &user, &chat, "second", Uuid::new_v4()).await;

    assert_aborted(&resp, "turn_already_running");
    assert_eq!(
        env.provider().chat_requests().len(),
        1,
        "no second provider request"
    );
    assert_eq!(turn_row_count(&env).await, 1, "no second turn");
    assert_eq!(
        message_row_count(&env).await,
        1,
        "only the running turn's user message"
    );
    stream.disconnect();
}

#[tokio::test]
async fn a_send_without_request_id_is_also_rejected_while_a_turn_is_running() {
    let (env, user, chat) = env_user_chat().await;
    let stream = live::start_hung_turn(&env, &user, &chat, Uuid::new_v4(), "first", &["par"]).await;

    let resp = common::send_text(&env, &user, &chat, "second").await;

    assert_aborted(&resp, "turn_already_running");
    stream.disconnect();
}

#[tokio::test]
async fn the_parallel_turn_rejection_is_a_json_problem_without_an_event_stream() {
    let (env, user, chat) = env_user_chat().await;
    let stream = live::start_hung_turn(&env, &user, &chat, Uuid::new_v4(), "first", &["par"]).await;

    let resp = common::send_text(&env, &user, &chat, "second").await;

    assert!(content_type(&resp).starts_with("application/problem+json"));
    stream.disconnect();
}

#[tokio::test]
async fn the_rejected_parallel_send_takes_no_quota_reserve_of_its_own() {
    let (env, user, chat) = env_user_chat().await;
    let running = Uuid::new_v4();
    let stream = live::start_hung_turn(&env, &user, &chat, running, "first", &["par"]).await;
    let reserved_by_running = total_reserved(&env).await;
    assert!(reserved_by_running > 0);

    common::send_text(&env, &user, &chat, "second").await;

    assert_eq!(total_reserved(&env).await, reserved_by_running);
    stream.disconnect();
}

#[tokio::test]
async fn a_new_turn_is_accepted_after_the_running_turn_was_cancelled() {
    let (env, user, chat) = env_user_chat().await;
    let running = Uuid::new_v4();
    let stream = live::start_hung_turn(&env, &user, &chat, running, "first", &["par"]).await;
    live::disconnect_and_wait_cancelled(&env, &chat, running, stream).await;

    let events = expect_stream(&send_with_id(&env, &user, &chat, "second", Uuid::new_v4()).await);

    assert_eq!(terminal(&events).event, "done");
    assert_eq!(turn_rows(&env, &chat).await.len(), 2);
}

#[tokio::test]
async fn a_new_turn_is_accepted_after_a_completed_turn() {
    let (env, user, chat) = env_user_chat().await;
    complete_turn(&env, &user, &chat, "first", "A").await;

    let events = common::send_ok(&env, &user, &chat, "second").await;

    assert_eq!(terminal(&events).event, "done");
}

#[tokio::test]
async fn a_new_turn_is_accepted_after_a_failed_turn() {
    let (env, user, chat) = env_user_chat().await;
    enqueue_chat(&env, failed_script("boom", None));
    common::send_text(&env, &user, &chat, "first").await;

    let events = common::send_ok(&env, &user, &chat, "second").await;

    assert_eq!(terminal(&events).event, "done");
}

#[tokio::test]
async fn at_most_one_turn_runs_at_a_time_per_chat_not_per_user() {
    let (env, user, chat) = env_user_chat().await;
    let other_chat = new_chat(&env, &user).await;
    let stream = live::start_hung_turn(&env, &user, &chat, Uuid::new_v4(), "first", &["par"]).await;

    let events = common::send_ok(&env, &user, &other_chat, "elsewhere").await;

    assert_eq!(terminal(&events).event, "done");
    stream.disconnect();
}

#[tokio::test]
async fn another_users_running_turn_does_not_block_this_user() {
    let env = env_two_models().await;
    let alice = TestUser::new();
    let bob = TestUser::in_tenant(alice.tenant_id);
    let alice_chat = new_chat(&env, &alice).await;
    let bob_chat = new_chat(&env, &bob).await;
    let stream =
        live::start_hung_turn(&env, &alice, &alice_chat, Uuid::new_v4(), "first", &["par"]).await;

    let events = common::send_ok(&env, &bob, &bob_chat, "mine").await;

    assert_eq!(terminal(&events).event, "done");
    stream.disconnect();
}

// ===========================================================================
// Check order: replay before the parallel-turn guard
// ===========================================================================

#[tokio::test]
async fn a_completed_replay_works_while_another_turn_of_the_chat_is_running() {
    let (env, user, chat) = env_user_chat().await;
    let completed = Uuid::new_v4();
    enqueue_text(&env, &["stored"], 10, 5);
    send_with_id(&env, &user, &chat, "first", completed).await;
    let running = Uuid::new_v4();
    let stream = live::start_hung_turn(&env, &user, &chat, running, "second", &["par"]).await;

    let resp = send_with_id(&env, &user, &chat, "first", completed).await;

    let replay = expect_stream(&resp);
    assert_eq!(
        started(&replay).data["is_new_turn"],
        json!(false),
        "{replay:?}"
    );
    assert_eq!(streamed_text(&replay), "stored");
    assert_eq!(terminal(&replay).event, "done");
    assert_eq!(
        live::turn_state(&env, &chat, running).await,
        "running",
        "the replay does not disturb the running turn"
    );
    stream.disconnect();
}

#[tokio::test]
async fn request_id_conflict_wins_over_turn_already_running() {
    let (env, user, chat) = env_user_chat().await;
    let failed = Uuid::new_v4();
    enqueue_chat(&env, failed_script("boom", None));
    send_with_id(&env, &user, &chat, "first", failed).await;
    let stream =
        live::start_hung_turn(&env, &user, &chat, Uuid::new_v4(), "second", &["par"]).await;

    let resp = send_with_id(&env, &user, &chat, "first", failed).await;

    assert_aborted(&resp, "request_id_conflict");
    stream.disconnect();
}

#[tokio::test]
async fn only_the_running_turns_own_request_id_conflicts_while_it_runs() {
    let (env, user, chat) = env_user_chat().await;
    let running = Uuid::new_v4();
    let stream = live::start_hung_turn(&env, &user, &chat, running, "first", &["par"]).await;

    let same_key = send_with_id(&env, &user, &chat, "again", running).await;
    let other_key = send_with_id(&env, &user, &chat, "other", Uuid::new_v4()).await;

    assert_aborted(&same_key, "request_id_conflict");
    assert_aborted(&other_key, "turn_already_running");
    stream.disconnect();
}

#[tokio::test]
async fn a_replay_leaves_the_running_turns_reserve_untouched() {
    let (env, user, chat) = env_user_chat().await;
    let completed = Uuid::new_v4();
    send_with_id(&env, &user, &chat, "first", completed).await;
    let stream =
        live::start_hung_turn(&env, &user, &chat, Uuid::new_v4(), "second", &["par"]).await;
    let reserved = total_reserved(&env).await;
    let rows_before = quota_rows(&env, &user).await;

    send_with_id(&env, &user, &chat, "first", completed).await;

    assert_eq!(total_reserved(&env).await, reserved);
    assert_eq!(quota_rows(&env, &user).await, rows_before);
    stream.disconnect();
}

#[tokio::test]
async fn a_replay_leaves_the_original_turn_row_unchanged() {
    let (env, user, chat) = env_user_chat().await;
    let request_id = Uuid::new_v4();
    send_with_id(&env, &user, &chat, "first", request_id).await;
    let before = turn_row(&env, &chat, request_id).await.unwrap();

    send_with_id(&env, &user, &chat, "first", request_id).await;

    let after = turn_row(&env, &chat, request_id).await.unwrap();
    assert_eq!(after.state, before.state);
    assert_eq!(after.id, before.id);
    assert_eq!(after.assistant_message_id, before.assistant_message_id);
    assert_eq!(after.has_completed_at, before.has_completed_at);
}

#[tokio::test]
async fn stream_request_id_of_a_replay_matches_the_requested_key() {
    let (env, user, chat) = env_user_chat().await;
    let request_id = Uuid::new_v4();
    send_with_id(&env, &user, &chat, "first", request_id).await;

    let replay = expect_stream(&send_with_id(&env, &user, &chat, "first", request_id).await);

    assert_eq!(stream_request_id(&replay), request_id);
}
