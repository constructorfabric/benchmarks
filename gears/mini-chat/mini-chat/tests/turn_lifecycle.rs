#![allow(clippy::unwrap_used, clippy::expect_used)]

//! Turn lifecycle (`running` -> `completed` / `failed` / `cancelled`) and the turn status API
//! `GET /v1/chats/{id}/turns/{request_id}` (DESIGN sections 3.3 "Turn Status API", 3.9 "Turn
//! State Model", 3.6 "Streaming Cancellation", 5.7 client disconnect rules).

mod common;
mod live;

use common::{
    CHAT_RESOURCE_TYPE, TURN_RESOURCE_TYPE, USAGE_QUEUE, assert_not_found, assert_problem,
    chat_request_body, complete_turn, enqueue_chat, env_user_chat, exec_sql, expect_stream,
    failed_script, get, hang_script, has_violation_reason, input_text, list_messages_ok,
    message_roles, message_rows, new_chat, only_turn, parse_ts, send_with_id, stream_request_id,
    turn_path, turn_row, uuid_hex,
};
use http::StatusCode;
use mini_chat::test_support::{AuthzMode, ScriptedResponse, TestEnv, TestEnvBuilder, TestUser};
use serde_json::{Value, json};
use uuid::Uuid;

async fn turn_status(env: &TestEnv, user: &TestUser, chat: &str, request_id: Uuid) -> Value {
    let resp = get(env, user, &turn_path(chat, &request_id.to_string())).await;
    common::expect_status(&resp, StatusCode::OK);
    resp.json()
}

fn assert_status_shape(status: &Value) {
    let allowed = [
        "request_id",
        "state",
        "error_code",
        "assistant_message_id",
        "updated_at",
    ];
    for key in status.as_object().unwrap().keys() {
        assert!(
            allowed.contains(&key.as_str()),
            "unexpected `{key}` in {status}"
        );
    }
    assert!(status["request_id"].is_string(), "{status}");
    assert!(status["state"].is_string(), "{status}");
    let _updated_at = parse_ts(&status["updated_at"]);
}

// ===========================================================================
// Turn status API
// ===========================================================================

#[tokio::test]
async fn a_completed_turn_is_done_with_its_assistant_message_id() {
    let (env, user, chat) = env_user_chat().await;
    let turn = complete_turn(&env, &user, &chat, "hello", "answer").await;

    let status = turn_status(&env, &user, &chat, turn.request_id).await;

    assert_status_shape(&status);
    assert_eq!(status["request_id"], json!(turn.request_id.to_string()));
    assert_eq!(status["state"], json!("done"));
    assert_eq!(status["assistant_message_id"], json!(turn.message_id));
    assert!(status.get("error_code").is_none(), "{status}");
}

#[tokio::test]
async fn a_running_turn_is_running_without_assistant_message_or_error_code() {
    let (env, user, chat) = env_user_chat().await;
    let request_id = Uuid::new_v4();
    let stream = live::start_hung_turn(&env, &user, &chat, request_id, "hello", &["par"]).await;

    let status = turn_status(&env, &user, &chat, request_id).await;

    assert_status_shape(&status);
    assert_eq!(status["state"], json!("running"));
    assert!(status.get("assistant_message_id").is_none(), "{status}");
    assert!(status.get("error_code").is_none(), "{status}");
    stream.disconnect();
}

#[tokio::test]
async fn a_failed_turn_is_error_with_its_error_code_and_no_assistant_message() {
    let (env, user, chat) = env_user_chat().await;
    let request_id = Uuid::new_v4();
    enqueue_chat(&env, failed_script("boom", None));
    send_with_id(&env, &user, &chat, "hello", request_id).await;

    let status = turn_status(&env, &user, &chat, request_id).await;

    assert_status_shape(&status);
    assert_eq!(status["state"], json!("error"));
    assert_eq!(status["error_code"], json!("provider_error"));
    assert!(status.get("assistant_message_id").is_none(), "{status}");
}

#[tokio::test]
async fn a_rate_limited_turn_reports_the_rate_limited_error_code() {
    let (env, user, chat) = env_user_chat().await;
    let request_id = Uuid::new_v4();
    enqueue_chat(
        &env,
        ScriptedResponse::Error {
            status: 429,
            body: json!({ "error": { "message": "slow down" } }),
            headers: vec![],
        },
    );
    send_with_id(&env, &user, &chat, "hello", request_id).await;

    let status = turn_status(&env, &user, &chat, request_id).await;

    assert_eq!(status["state"], json!("error"));
    assert_eq!(status["error_code"], json!("rate_limited"));
}

#[tokio::test]
async fn a_cancelled_turn_with_partial_content_exposes_the_partial_assistant_message() {
    let (env, user, chat) = env_user_chat().await;
    let request_id = Uuid::new_v4();
    let stream = live::start_hung_turn(
        &env,
        &user,
        &chat,
        request_id,
        "hello",
        &["partial ", "text"],
    )
    .await;
    live::disconnect_and_wait_cancelled(&env, &chat, request_id, stream).await;

    let status = turn_status(&env, &user, &chat, request_id).await;

    assert_status_shape(&status);
    assert_eq!(status["state"], json!("cancelled"));
    assert!(status.get("error_code").is_none(), "{status}");
    let rows = message_rows(&env, &chat).await;
    let assistant = rows
        .iter()
        .find(|row| row.role == "assistant")
        .expect("a partial message");
    assert_eq!(assistant.content, "partial text");
    assert_eq!(
        status["assistant_message_id"],
        json!(assistant.id.to_string())
    );
}

#[tokio::test]
async fn a_cancelled_turn_without_content_has_no_assistant_message_id() {
    let (env, user, chat) = env_user_chat().await;
    let request_id = Uuid::new_v4();
    let stream = live::start_hung_turn(&env, &user, &chat, request_id, "hello", &[]).await;
    live::disconnect_and_wait_cancelled(&env, &chat, request_id, stream).await;

    let status = turn_status(&env, &user, &chat, request_id).await;

    assert_eq!(status["state"], json!("cancelled"));
    assert!(status.get("assistant_message_id").is_none(), "{status}");
    let turn = only_turn(&env, &chat).await;
    assert_eq!(turn.assistant_message_id, None);
    let roles: Vec<String> = message_rows(&env, &chat)
        .await
        .into_iter()
        .map(|m| m.role)
        .collect();
    assert_eq!(
        roles,
        ["user"],
        "no empty assistant message is persisted on cancel"
    );
}

#[tokio::test]
async fn turn_status_never_exposes_billing_or_provider_details() {
    let (env, user, chat) = env_user_chat().await;
    let turn = complete_turn(&env, &user, &chat, "hello", "answer").await;

    let resp = get(&env, &user, &turn_path(&chat, &turn.request_id.to_string())).await;

    let text = common::body_text(&resp);
    for forbidden in [
        "resp_",
        "billing",
        "settlement",
        "credits",
        "chat_id",
        "reserve",
        "provider",
    ] {
        assert!(
            !text.contains(forbidden),
            "`{forbidden}` must not be exposed: {text}"
        );
    }
}

#[tokio::test]
async fn an_unknown_request_id_is_404_naming_the_turn_resource() {
    let (env, user, chat) = env_user_chat().await;

    let resp = get(&env, &user, &turn_path(&chat, &Uuid::new_v4().to_string())).await;

    assert_not_found(&resp, TURN_RESOURCE_TYPE);
}

#[tokio::test]
async fn another_users_turn_is_404() {
    let (env, owner, chat) = env_user_chat().await;
    let turn = complete_turn(&env, &owner, &chat, "hello", "answer").await;
    let intruder = TestUser::in_tenant(owner.tenant_id);

    let resp = get(
        &env,
        &intruder,
        &turn_path(&chat, &turn.request_id.to_string()),
    )
    .await;

    let problem = assert_problem(&resp, 404, Some("not_found"));
    let resource_type = problem["context"]["resource_type"].as_str().unwrap();
    assert!(
        resource_type == TURN_RESOURCE_TYPE || resource_type == CHAT_RESOURCE_TYPE,
        "{problem}"
    );
}

#[tokio::test]
async fn another_tenants_turn_is_404() {
    let (env, owner, chat) = env_user_chat().await;
    let turn = complete_turn(&env, &owner, &chat, "hello", "answer").await;

    let resp = get(
        &env,
        &TestUser::new(),
        &turn_path(&chat, &turn.request_id.to_string()),
    )
    .await;

    assert_problem(&resp, 404, Some("not_found"));
}

#[tokio::test]
async fn a_turn_of_a_different_chat_is_404_in_this_chat() {
    let (env, user, chat) = env_user_chat().await;
    let other = new_chat(&env, &user).await;
    let turn = complete_turn(&env, &user, &other, "hello", "answer").await;

    let resp = get(&env, &user, &turn_path(&chat, &turn.request_id.to_string())).await;

    assert_not_found(&resp, TURN_RESOURCE_TYPE);
}

#[tokio::test]
async fn a_soft_deleted_turn_is_404() {
    let (env, user, chat) = env_user_chat().await;
    let turn = complete_turn(&env, &user, &chat, "hello", "answer").await;
    exec_sql(
        &env,
        &format!(
            "UPDATE chat_turns SET deleted_at = '2000-01-01T00:00:00Z' WHERE hex(request_id) = '{}'",
            uuid_hex(turn.request_id)
        ),
    )
    .await;

    let resp = get(&env, &user, &turn_path(&chat, &turn.request_id.to_string())).await;

    assert_not_found(&resp, TURN_RESOURCE_TYPE);
}

#[tokio::test]
async fn an_unknown_chat_is_404() {
    let env = TestEnv::new().await;
    let user = TestUser::new();

    let resp = get(
        &env,
        &user,
        &turn_path(&Uuid::new_v4().to_string(), &Uuid::new_v4().to_string()),
    )
    .await;

    assert_problem(&resp, 404, Some("not_found"));
}

#[tokio::test]
async fn a_non_uuid_request_id_is_400_invalid_path_params() {
    let (env, user, chat) = env_user_chat().await;

    let resp = get(&env, &user, &turn_path(&chat, "not-a-uuid")).await;

    let problem = assert_problem(&resp, 400, Some("invalid_argument"));
    assert!(
        has_violation_reason(&problem, "invalid_path_params"),
        "{problem}"
    );
}

#[tokio::test]
async fn a_denied_turn_status_request_is_403() {
    let env = TestEnvBuilder::new().authz(AuthzMode::Deny).build().await;

    let resp = get(
        &env,
        &TestUser::new(),
        &turn_path(&Uuid::new_v4().to_string(), &Uuid::new_v4().to_string()),
    )
    .await;

    assert_problem(&resp, 403, Some("permission_denied"));
}

#[tokio::test]
async fn the_status_of_a_replayed_request_id_is_still_done() {
    let (env, user, chat) = env_user_chat().await;
    let turn = complete_turn(&env, &user, &chat, "hello", "answer").await;
    send_with_id(&env, &user, &chat, "hello", turn.request_id).await;

    let status = turn_status(&env, &user, &chat, turn.request_id).await;

    assert_eq!(status["state"], json!("done"));
}

#[tokio::test]
async fn a_new_turn_after_a_409_can_be_recovered_through_the_status_endpoint() {
    let (env, user, chat) = env_user_chat().await;
    let running = Uuid::new_v4();
    let stream = live::start_hung_turn(&env, &user, &chat, running, "first", &["par"]).await;
    let rejected = common::send_text(&env, &user, &chat, "second").await;
    assert_eq!(rejected.status, StatusCode::CONFLICT);

    let status = turn_status(&env, &user, &chat, running).await;

    assert_eq!(
        status["state"],
        json!("running"),
        "recovery step 4: still generating"
    );
    stream.disconnect();
}

// ===========================================================================
// Lifecycle rows
// ===========================================================================

#[tokio::test]
async fn a_running_turn_row_has_no_completion_time_and_a_persisted_reserve() {
    let (env, user, chat) = env_user_chat().await;
    let request_id = Uuid::new_v4();
    let stream = live::start_hung_turn(&env, &user, &chat, request_id, "hello", &["par"]).await;

    let turn = turn_row(&env, &chat, request_id).await.unwrap();

    assert_eq!(turn.state, "running");
    assert!(!turn.has_completed_at, "a running turn has no completed_at");
    assert!(
        turn.reserve_tokens.is_some(),
        "the reserve is persisted before the provider call"
    );
    assert_eq!(turn.assistant_message_id, None);
    stream.disconnect();
}

#[tokio::test]
async fn the_user_message_is_persisted_before_the_provider_answers() {
    let (env, user, chat) = env_user_chat().await;
    let request_id = Uuid::new_v4();
    let stream =
        live::start_hung_turn(&env, &user, &chat, request_id, "my question", &["par"]).await;

    let rows = message_rows(&env, &chat).await;

    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].role, "user");
    assert_eq!(rows[0].content, "my question");
    assert_eq!(rows[0].request_id, Some(request_id));
    stream.disconnect();
}

#[tokio::test]
async fn a_client_disconnect_finalizes_the_turn_as_cancelled_with_a_completion_time() {
    let (env, user, chat) = env_user_chat().await;
    let request_id = Uuid::new_v4();
    let stream = live::start_hung_turn(&env, &user, &chat, request_id, "hello", &["par"]).await;

    live::disconnect_and_wait_cancelled(&env, &chat, request_id, stream).await;

    let turn = turn_row(&env, &chat, request_id).await.unwrap();
    assert_eq!(turn.state, "cancelled");
    assert!(turn.has_completed_at);
}

#[tokio::test]
async fn a_disconnect_before_any_content_still_cancels_the_turn() {
    let (env, user, chat) = env_user_chat().await;
    let request_id = Uuid::new_v4();
    enqueue_chat(&env, hang_script(&[]));
    let mut stream = live::open(
        &env,
        &user,
        &chat,
        json!({ "content": "hello", "request_id": request_id.to_string() }),
    )
    .await;
    live::read_until(&mut stream, "stream_started").await;

    live::disconnect_and_wait_cancelled(&env, &chat, request_id, stream).await;

    assert_eq!(live::turn_state(&env, &chat, request_id).await, "cancelled");
}

#[tokio::test]
async fn a_cancelled_turn_releases_the_chat_for_a_new_turn() {
    let (env, user, chat) = env_user_chat().await;
    let request_id = Uuid::new_v4();
    let stream = live::start_hung_turn(&env, &user, &chat, request_id, "hello", &["par"]).await;
    live::disconnect_and_wait_cancelled(&env, &chat, request_id, stream).await;

    let events = common::send_ok(&env, &user, &chat, "again").await;

    assert_eq!(common::terminal(&events).event, "done");
}

#[tokio::test]
async fn a_disconnect_after_the_terminal_event_leaves_the_turn_completed() {
    let (env, user, chat) = env_user_chat().await;
    common::enqueue_text(&env, &["answer"], 10, 5);
    let mut stream = live::open(&env, &user, &chat, json!({ "content": "hello" })).await;
    let events = live::read_until(&mut stream, "done").await;
    let request_id = stream_request_id(&events);

    stream.disconnect();

    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    let turn = turn_row(&env, &chat, request_id).await.unwrap();
    assert_eq!(
        turn.state, "completed",
        "a disconnect after done is a no-op"
    );
    assert_eq!(
        env.outbox_payloads(USAGE_QUEUE).await.len(),
        1,
        "no second settlement"
    );
}

#[tokio::test]
async fn the_partial_message_of_a_cancelled_turn_appears_in_the_messages_list() {
    let (env, user, chat) = env_user_chat().await;
    let request_id = Uuid::new_v4();
    let stream =
        live::start_hung_turn(&env, &user, &chat, request_id, "hello", &["half an answer"]).await;
    live::disconnect_and_wait_cancelled(&env, &chat, request_id, stream).await;

    let page = list_messages_ok(&env, &user, &chat, &[]).await;

    assert_eq!(message_roles(&page), ["user", "assistant"]);
    assert_eq!(common::message_contents(&page)[1], "half an answer");
    assert_eq!(
        common::items(&page)[1]["request_id"],
        json!(request_id.to_string())
    );
}

#[tokio::test]
async fn the_partial_message_of_a_cancelled_turn_is_part_of_the_next_turns_history() {
    let (env, user, chat) = env_user_chat().await;
    let request_id = Uuid::new_v4();
    let stream = live::start_hung_turn(
        &env,
        &user,
        &chat,
        request_id,
        "first question",
        &["half an answer"],
    )
    .await;
    live::disconnect_and_wait_cancelled(&env, &chat, request_id, stream).await;

    common::send_ok(&env, &user, &chat, "second question").await;

    let body = chat_request_body(&env, 1);
    let input = input_text(&body);
    assert!(input.contains("first question"), "{input}");
    assert!(
        input.contains("half an answer"),
        "the partial answer is history: {input}"
    );
    assert!(input.contains("second question"), "{input}");
}

#[tokio::test]
async fn a_failed_turn_keeps_its_user_message_in_the_conversation() {
    let (env, user, chat) = env_user_chat().await;
    enqueue_chat(&env, failed_script("boom", None));
    common::send_text(&env, &user, &chat, "will fail").await;

    let rows = message_rows(&env, &chat).await;

    assert_eq!(rows[0].role, "user");
    assert_eq!(rows[0].content, "will fail");
}

#[tokio::test]
async fn provider_failure_finalizes_the_turn_failed_with_a_completion_time() {
    let (env, user, chat) = env_user_chat().await;
    enqueue_chat(&env, failed_script("boom", None));
    let events = expect_stream(&common::send_text(&env, &user, &chat, "hello").await);

    let turn = turn_row(&env, &chat, stream_request_id(&events))
        .await
        .unwrap();

    assert_eq!(turn.state, "failed");
    assert_eq!(turn.error_code.as_deref(), Some("provider_error"));
    assert!(turn.has_completed_at);
}

#[tokio::test]
async fn every_terminal_turn_has_a_completion_time_and_a_running_turn_has_none() {
    let (env, user, chat) = env_user_chat().await;
    complete_turn(&env, &user, &chat, "one", "A").await;
    enqueue_chat(&env, failed_script("boom", None));
    common::send_text(&env, &user, &chat, "two").await;
    let cancelled = Uuid::new_v4();
    let stream = live::start_hung_turn(&env, &user, &chat, cancelled, "three", &["p"]).await;
    live::disconnect_and_wait_cancelled(&env, &chat, cancelled, stream).await;

    let turns = common::turn_rows(&env, &chat).await;

    let states: Vec<&str> = turns.iter().map(|t| t.state.as_str()).collect();
    assert_eq!(states, ["completed", "failed", "cancelled"]);
    assert!(turns.iter().all(|turn| turn.has_completed_at), "{turns:?}");
}
